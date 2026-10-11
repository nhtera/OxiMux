//! The hub's side of device state: applying a change (for an agent's verb,
//! once consent is settled, and for the panel's Device menu), erasing a
//! device — shut down, wiped, booted again through the registry so the panel
//! follows — and Android's demo-mode status bar, which must not outlive the
//! worktree that turned it on.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{Context, Task};
use oximux_simulator::android::adb::Adb;
use oximux_simulator::android::{Target, devices};
use oximux_simulator::runner::Runner;
use oximux_simulator::state::StateChange;
use oximux_simulator::{DeviceId, Source, android, simctl_state};

use super::{NoticeKind, SIMCTL_TIMEOUT, SimulatorHub};

/// One `adb` or `simctl` state call.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an erase waits for the device to shut down.
const ERASE_WAIT: Duration = Duration::from_secs(60);

/// What the hub remembers about device state.
#[derive(Default)]
pub(crate) struct Memo {
    /// Emulators to boot with their data wiped (an erase in progress).
    wipe_next: HashSet<DeviceId>,
    /// Android devices showing demo mode's status bar.
    demo: HashSet<DeviceId>,
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
                let job = Box::new(move || {
                    let adb = Adb::new(runner.as_ref(), &adb_path);
                    android::state::apply(&adb, &serial, &android, &change, STATE_TIMEOUT).map_err(|e| e.to_string())
                });
                return cx.spawn(async move |this, cx| {
                    let done = cx.background_executor().spawn(async move { job() }).await;
                    if let (Ok(()), Some(on)) = (&done, demo) {
                        let _ = this.update(cx, |hub, _| {
                            if on { hub.device_state.demo.insert(target) } else { hub.device_state.demo.remove(&target) }
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
    /// boot has started. A real device is never erased.
    pub(crate) fn erase(&mut self, udid: &DeviceId, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if udid.is_physical() {
            return Task::ready(Err("a real device is never erased by OxiMux".into()));
        }
        let runner = self.runner.clone();
        let wipe: Box<dyn FnOnce() -> Result<(), String> + Send> = match udid.source() {
            Source::Simctl => {
                let Some(sim) = udid.sim_udid().filter(|_| self.watch_gate().xcode_ok) else {
                    return Task::ready(Err("Xcode was not found".into()));
                };
                Box::new(move || simctl_state::erase(runner.as_ref(), &sim, SIMCTL_TIMEOUT, ERASE_WAIT).map_err(|e| e.to_string()))
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
        self.shutdown_after_recording(udid, cx);
        let target = udid.clone();
        cx.spawn(async move |this, cx| {
            let wiped = cx.background_executor().spawn(async move { wipe() }).await;
            let _ = this.update(cx, |hub, cx| {
                if wiped.is_ok() && target.source() == Source::Adb {
                    hub.device_state.wipe_next.insert(target.clone());
                }
                hub.device_state.demo.remove(&target);
                // Booted again either way: a failed wipe must not leave the
                // device off under its panel.
                hub.reconnect(&target, cx);
                if let Err(e) = &wiped {
                    hub.notice(&target, NoticeKind::Error, format!("The device was not erased: {e}"), cx);
                }
            });
            wiped
        })
    }

    /// Whether `udid`'s next boot wipes its data (taken once).
    pub(super) fn take_wipe(&mut self, udid: &DeviceId) -> bool {
        self.device_state.wipe_next.remove(udid)
    }

    /// `udid` lost a worktree: an Android device in demo mode gets its real
    /// status bar back.
    pub(super) fn leave_demo_mode(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if !self.device_state.demo.remove(udid) {
            return;
        }
        let (Some(sdk), Some(serial)) = (self.android_sdk.clone(), self.session(udid).and_then(|s| s.android().map(|a| a.serial().to_owned())))
        else {
            return;
        };
        let runner = self.runner.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = android::state::exit_demo_mode(&Adb::new(runner.as_ref(), &sdk.adb()), &serial, STATE_TIMEOUT) {
                    tracing::warn!("demo mode left on: {e}");
                }
            })
            .detach();
    }
}

/// Until the emulator `target` has stopped (≤ [`ERASE_WAIT`]).
fn wait_stopped(runner: &dyn Runner, sdk: &android::sdk::Sdk, target: &Target) -> Result<(), String> {
    let deadline = Instant::now() + ERASE_WAIT;
    while devices::serial_of(runner, sdk, target, Duration::from_secs(5)).map_err(|e| e.to_string())?.is_some() {
        if Instant::now() >= deadline {
            return Err("the emulator did not shut down".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}
