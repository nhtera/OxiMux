//! The hub's Android side (P10): finding the SDK, listing AVDs and phones
//! beside the iOS simulators, booting an AVD, starting a scrcpy session, and
//! shutting an emulator down. The registry, the watcher and the viewers treat
//! Android devices like simulators; what differs is only how each effect is
//! carried out, which is what this file owns.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gpui::Context;
use oximux_simulator::android::scrcpy_server::{self, StreamOptions};
use oximux_simulator::android::sdk::{self, Sdk};
use oximux_simulator::android::session::AndroidSession;
use oximux_simulator::android::{Target, devices};
use oximux_simulator::registry::{BootResult, Generation, Phase};
use oximux_simulator::runner::Runner;
use oximux_simulator::session::StreamSession;
use oximux_simulator::{DeviceId, DeviceInfo, Result, SimError, simctl};

use super::{HubEvent, SimulatorHub, simulator_dir};
use crate::app_settings::simulator_settings::Resolution;

const ADB_TIMEOUT: Duration = Duration::from_secs(15);

/// Every device this Mac can show: the iOS simulators (only with a resolvable
/// Xcode — never `xcrun` without one) and the Android devices (only with an
/// SDK). One side failing still lists the other.
pub(crate) fn list_all(runner: &(dyn Runner + Sync), xcode_ok: bool, sdk: Option<&Sdk>, timeout: Duration) -> Result<Vec<DeviceInfo>> {
    // Side by side: a slow adb must not hold up the simulators.
    let (ios, android) = std::thread::scope(|scope| {
        let android = scope.spawn(|| sdk.map(|sdk| devices::list(runner, sdk, devices::avd_home().as_deref(), timeout)));
        let ios = xcode_ok.then(|| simctl::list_devices(runner, timeout));
        (ios, android.join().unwrap_or(None))
    });
    match (ios, android) {
        (None, None) => Ok(Vec::new()),
        (Some(Err(e)), None | Some(Err(_))) | (None, Some(Err(e))) => Err(e),
        (ios, android) => {
            let mut all = ios.and_then(|r| r.inspect_err(|e| tracing::debug!("iOS listing: {e}")).ok()).unwrap_or_default();
            all.extend(android.and_then(|r| r.inspect_err(|e| tracing::debug!("Android listing: {e}")).ok()).unwrap_or_default());
            Ok(all)
        }
    }
}

/// Every booted device, for the watcher.
pub(crate) fn booted_all(runner: &dyn Runner, xcode_ok: bool, sdk: Option<&Sdk>) -> Result<BTreeSet<DeviceId>> {
    let mut all = if xcode_ok { oximux_simulator::boot_watch::list_booted(runner)? } else { BTreeSet::new() };
    if let Some(sdk) = sdk {
        // A failed listing fails the round (the watcher skips it): read as
        // "nothing booted", it would disconnect every Android device.
        all.extend(devices::booted_ids(runner, sdk, ADB_TIMEOUT)?);
    }
    Ok(all)
}

/// The Android SDK, as Settings and the environment point to it.
pub(crate) fn discover(configured: Option<&str>) -> Option<Sdk> {
    sdk::discover_here(configured.map(std::path::Path::new))
}

impl SimulatorHub {
    /// The Android SDK, once found (`None`: Android devices are not offered).
    pub fn android_sdk(&self) -> Option<&Sdk> {
        self.android_sdk.as_ref()
    }

    /// Look for the SDK again (Settings changed its folder), off the UI thread.
    pub fn refresh_android_sdk(&mut self, cx: &mut Context<Self>) {
        let configured = crate::shell::simulator::panel::settings(cx).android_sdk;
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { discover(configured.as_deref()) }).await;
            let _ = this.update(cx, |hub, cx| {
                if hub.android_sdk != found {
                    hub.android_sdk = found;
                    hub.refresh_devices(cx);
                    cx.emit(HubEvent::Availability);
                }
            });
        })
        .detach();
    }

    /// Boot an Android device: start an AVD headless, or confirm a phone is
    /// connected (a phone cannot be booted from here).
    pub(super) fn boot_android(&mut self, udid: DeviceId, generation: Generation, cancel: Arc<AtomicBool>, cx: &mut Context<Self>) {
        let (Some(sdk), Some(target)) = (self.android_sdk.clone(), Target::from_id(&udid)) else {
            self.finish_boot(udid, generation, BootResult::Failed("The Android SDK was not found.".into()), cx);
            return;
        };
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match &target {
                        Target::Avd(name) => match devices::boot_avd(runner.as_ref(), &sdk, name, &cancel) {
                            Ok(devices::BootOutcome::Booted(_)) => BootResult::Booted,
                            // Someone else started it meanwhile: not ours to shut down.
                            Ok(devices::BootOutcome::AlreadyBooted(_)) => BootResult::AlreadyBooted,
                            Err(SimError::Cancelled) => BootResult::Cancelled,
                            Err(e) => BootResult::Failed(e.to_string()),
                        },
                        Target::Serial(_) => match devices::serial_of(runner.as_ref(), &sdk, &target, ADB_TIMEOUT) {
                            Ok(Some(_)) => BootResult::AlreadyBooted,
                            _ => BootResult::Failed("The phone is not connected (or USB debugging is not allowed on it).".into()),
                        },
                    }
                })
                .await;
            let _ = this.update(cx, |hub, cx| hub.finish_boot(udid, generation, result, cx));
        })
        .detach();
    }

    fn finish_boot(&mut self, udid: DeviceId, generation: Generation, result: BootResult, cx: &mut Context<Self>) {
        let effects = self.registry.boot_finished(&udid, generation, result);
        self.run(effects, cx);
        self.refresh_devices(cx);
        cx.emit(HubEvent::Changed(udid));
    }

    /// Start streaming a running Android device: fetch the pinned scrcpy
    /// server on first use, find the device's serial, start the server.
    pub(super) fn start_android_session(&mut self, udid: DeviceId, generation: Generation, cx: &mut Context<Self>) {
        let sdk = self.android_sdk.clone();
        let settings = crate::shell::simulator::panel::settings(cx).stream;
        let opts = StreamOptions {
            // The long side: half of a phone's ~2400 px, or its own size.
            max_size: match settings.resolution {
                Resolution::Half => 1280,
                Resolution::Full => 0,
            },
            max_fps: u16::try_from(settings.fps).unwrap_or(60),
        };
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let target = udid.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let sdk = sdk.ok_or_else(|| ("The Android SDK was not found.".to_owned(), false))?;
                    let android = Target::from_id(&target).ok_or_else(|| ("not an Android device".to_owned(), false))?;
                    let serial = devices::serial_of(runner.as_ref(), &sdk, &android, ADB_TIMEOUT)
                        .map_err(|e| (e.to_string(), false))?
                        .ok_or_else(|| ("The device is not running.".to_owned(), true))?;
                    let jar = scrcpy_server::ensure_jar(&simulator_dir().join("scrcpy")).map_err(|e| (e.to_string(), false))?;
                    AndroidSession::start(&sdk.adb(), &jar, target, &serial, opts)
                        .map(StreamSession::from)
                        .map_err(|e| (e.to_string(), false))
                })
                .await;
            let _ = this.update(cx, |hub, cx| hub.finish_start(udid, generation, result, cx));
        })
        .detach();
    }

    /// Shut an emulator down (the idle timer, the power button). A phone is
    /// never shut down.
    pub(super) fn shutdown_android(&self, udid: &DeviceId, cx: &mut Context<Self>) {
        let (Some(sdk), Some(target)) = (self.android_sdk.clone(), Target::from_id(udid)) else { return };
        let (runner, udid) = (self.runner.clone(), udid.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { devices::shutdown(runner.as_ref(), &sdk, &target, ADB_TIMEOUT) })
                .await;
            if let Err(e) = result {
                tracing::warn!("android shutdown failed: {e}");
                // Still running: say so, and let agents use it again.
                let _ = this.update(cx, |hub, cx| {
                    hub.clear_stopped_by_user(&udid);
                    cx.emit(HubEvent::Notice(udid, super::NoticeKind::Error, format!("The emulator did not shut down: {e}")));
                });
            }
        })
        .detach();
    }

    /// `adb -s <serial> logcat` for a streaming Android device, for a
    /// terminal tab. `None` without a session (the serial is only known then)
    /// or with a serial that is not safe on a command line.
    pub fn logcat_script(&self, udid: &DeviceId) -> Option<String> {
        let session = self.session(udid)?;
        let serial = session.android()?.serial().to_owned();
        let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'));
        let adb = self.android_sdk.as_ref()?.adb();
        let adb = adb.to_str().filter(|p| !p.contains('\'') && !p.contains('\n'))?.to_owned();
        safe(&serial).then(|| format!("'{adb}' -s {serial} logcat -v brief"))
    }

    /// Start `screenrecord` on a streaming Android device (the serial is the
    /// session's). Stopping pulls the movie to the Desktop like the iOS one.
    pub(super) fn start_android_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let serial = self.session(udid).and_then(|s| s.android().map(|a| a.serial().to_owned()));
        let (Some(serial), Some(sdk)) = (serial, self.android_sdk.clone()) else { return };
        if !self.recording_starts.insert(udid.clone()) {
            return;
        }
        let (device, stamp) = (self.device_name(udid), super::stamp());
        // screenrecord writes MP4 (iOS recordings are QuickTime).
        let path = super::capture_path(&super::capture_dir(), super::CaptureKind::Recording, &device, &stamp).with_extension("mp4");
        let (ledger, id) = (self.ledger.clone(), udid.clone());
        let started = oximux_simulator::record::Recording::start_android(&sdk.adb(), &serial, &id, &path, ledger);
        self.recording_starts.remove(udid);
        match started {
            Ok(recording) => {
                let since = recording.started;
                self.recordings.insert(udid.clone(), recording);
                self.auto_stop(udid.clone(), since, cx);
                cx.emit(HubEvent::Changed(udid.clone()));
            }
            Err(e) => cx.emit(HubEvent::Notice(
                udid.clone(),
                super::NoticeKind::Error,
                format!("Recording failed to start: {e}"),
            )),
        }
    }

    /// Whether `udid` is a phone (never booted or shut down by us).
    pub fn is_phone(udid: &DeviceId) -> bool {
        matches!(Target::from_id(udid), Some(Target::Serial(_)))
    }

    /// The phase a session start for `udid` found (for the "gone" check).
    pub(super) fn starting(&self, udid: &DeviceId, generation: Generation) -> bool {
        self.registry.phase(udid) == (Phase::Starting { generation })
    }
}
