//! The hub's real-iPhone side (Phase 6, view-only): listing USB iPhones with
//! `devicectl`, watching them come and go, streaming one through the capture
//! helper, and recording it. The registry treats an iPhone like any device
//! it never boots or shuts down; what differs is only how each effect is
//! carried out, which is what this file owns.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::Context;
use oximux_simulator::availability::{self, HelperStatus};
use oximux_simulator::helper::{HelperKind, HelperOptions};
use oximux_simulator::ios_device::DeviceSession;
use oximux_simulator::record::Recording;
use oximux_simulator::registry::Generation;
use oximux_simulator::runner::Runner;
use oximux_simulator::session::{HelperSession, StreamSession};
use oximux_simulator::{DeviceId, DeviceInfo, SimError, devicectl};

use super::{CaptureKind, HubEvent, NoticeKind, SimulatorHub, capture_dir, capture_path, simulator_dir, stamp};

/// `devicectl list devices` took 30 ms when measured (Xcode 26, one phone);
/// this bounds a wedged CoreDevice. It runs beside the other listings, never
/// in front of them.
pub(super) const DEVICECTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a cabled iPhone cannot be shown by this build: it has no capture app.
pub const NO_CAPTURE_APP: &str = "This build of OxiMux cannot show an iPhone's screen yet";

/// What the panel says when macOS keeps the camera from the capture helper
/// (the panel offers to open the Camera settings beside it).
pub const CAMERA_DENIED: &str =
    "OxiMux Device Capture needs Camera access to show the iPhone's screen (macOS lists phone screens as cameras).";
/// What the panel says when another app holds the iPhone's screen.
pub const DEVICE_BUSY: &str = "Another app is showing this iPhone's screen. Close it and try again.";

/// The real iPhones, for the device menu and the watcher. A failed listing
/// is `None` (no news), never "every iPhone unplugged".
pub(crate) fn list(runner: &dyn Runner) -> Option<Vec<DeviceInfo>> {
    let mut iphones = devicectl::list(runner, DEVICECTL_TIMEOUT).inspect_err(|e| tracing::debug!("iPhone listing: {e}")).ok()?;
    // Without the capture app an iPhone is listed, never offered.
    if matches!(availability::default_capture_probe(), HelperStatus::Missing(_)) {
        for phone in iphones.iter_mut().filter(|p| p.is_available) {
            phone.is_available = false;
            phone.state = oximux_simulator::DeviceState::Other("Unavailable".into());
            phone.note = Some(NO_CAPTURE_APP.into());
        }
    }
    Some(iphones)
}

/// What the watcher compares between rounds: each iPhone's id and whether
/// it can be shown (cabled and trusting this Mac).
pub(crate) fn states(iphones: &[DeviceInfo]) -> BTreeMap<DeviceId, bool> {
    iphones.iter().map(|d| (d.udid.clone(), d.is_available)).collect()
}

/// The panel's words for a capture helper that would not start.
fn start_error(e: &SimError) -> String {
    match e {
        SimError::CameraDenied(_) => CAMERA_DENIED.to_owned(),
        SimError::DeviceBusy(_) => DEVICE_BUSY.to_owned(),
        other => other.to_string(),
    }
}

/// A file-name-safe form of `udid` (`iosdev:` has a colon).
fn log_name(udid: &DeviceId) -> String {
    udid.as_str().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

impl SimulatorHub {
    /// The watcher's latest view of the iPhones. A change — one plugged in,
    /// unplugged, or trusting this Mac — refreshes the device list and says
    /// so (the first view too: the list may predate it).
    pub(super) fn observe_iphones(&mut self, now: BTreeMap<DeviceId, bool>, cx: &mut Context<Self>) {
        let before = self.iphone_states.replace(now);
        if before.is_none_or(|before| Some(&before) != self.iphone_states.as_ref()) {
            tracing::info!("iPhone watch: an iPhone was plugged in, unplugged or trusted this Mac");
            self.refresh_devices(cx);
            cx.emit(HubEvent::PhysicalChanged);
        }
    }

    /// Stream `udid` through the capture helper. The helper may wait in the
    /// Camera prompt on first use (see `HelperKind::ready_timeout`); that wait
    /// is on the background executor, never here.
    pub(super) fn start_iphone_session(&mut self, udid: DeviceId, generation: Generation, cx: &mut Context<Self>) {
        let stream = crate::shell::simulator::panel::settings(cx).stream;
        // One capture helper per phone: a start still waiting gives way.
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(older) = self.capture_starts.insert(udid.clone(), cancel.clone()) {
            older.store(true, Ordering::Release);
        }
        let mine = cancel.clone();
        let opts = HelperOptions {
            cancel: Some(cancel),
            kind: HelperKind::DeviceCapture,
            scale: f64::from(stream.effective_scale()),
            fps: f64::from(stream.fps),
            format: stream.encoding.format(),
            log_path: Some(simulator_dir().join("logs").join(format!("capture-{}.log", log_name(&udid)))),
            ledger: self.ledger.clone(),
            ..HelperOptions::default()
        };
        let reaped = self.reaped.clone();
        cx.spawn(async move |this, cx| {
            let target = udid.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = match availability::default_capture_probe() {
                        HelperStatus::Found(path) => path,
                        HelperStatus::Missing(why) => return Err((why, false)),
                    };
                    let (done, cvar) = &*reaped;
                    let _unused = cvar.wait_while(done.lock().unwrap(), |done| !*done).unwrap();
                    HelperSession::start(&path, &target, &opts)
                        .map(|video| StreamSession::from(DeviceSession::new(video)))
                        // Unplugged (or locked away) meanwhile: a disconnect.
                        .map_err(|e| (start_error(&e), matches!(e, SimError::DeviceNotBooted)))
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                if hub.capture_starts.get(&udid).is_some_and(|c| Arc::ptr_eq(c, &mine)) {
                    hub.capture_starts.remove(&udid);
                }
                hub.finish_start(udid, generation, result, cx);
            });
        })
        .detach();
    }

    /// Give up the capture starts nobody waits for any more (detached,
    /// reconnected, superseded): their helper is killed, and with it any
    /// Camera prompt it raised.
    pub(super) fn cancel_stale_capture_starts(&mut self) {
        let registry = &self.registry;
        self.capture_starts.retain(|udid, cancel| {
            let wanted = matches!(registry.phase(udid), oximux_simulator::registry::Phase::Starting { .. });
            if !wanted {
                cancel.store(true, Ordering::Release);
            }
            wanted
        });
    }

    /// Record a streaming iPhone through its capture helper. The movie is
    /// written in OxiMux's folder and moved to the captures folder when done.
    pub(super) fn start_iphone_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let Some(session) = self.session(udid).and_then(|s| s.ios_device().cloned()) else {
            return cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Error, "Show the iPhone first: it records what it streams.".into()));
        };
        if !self.recording_starts.insert(udid.clone()) {
            return; // a start is already in flight
        }
        let (device, stamp) = (self.device_name(udid), stamp());
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move {
                    // Probes by writing (may wait on the Desktop prompt): here.
                    let path = capture_path(&capture_dir(), CaptureKind::Recording, &device, &stamp);
                    Recording::start_capture(session, &simulator_dir().join("captures").join("staging"), &path)
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                let wanted = hub.recording_starts.remove(&udid);
                match started {
                    // Cancelled meanwhile: finalize what started.
                    Ok(recording) if !wanted => {
                        cx.background_executor().spawn(async move { drop(recording.stop(oximux_simulator::record::FINALIZE_GRACE)) }).detach();
                    }
                    Ok(recording) => {
                        let since = recording.started;
                        hub.recordings.insert(udid.clone(), recording);
                        hub.auto_stop(udid.clone(), since, cx);
                        cx.emit(HubEvent::Changed(udid));
                    }
                    Err(e) => cx.emit(HubEvent::Notice(udid, NoticeKind::Error, format!("Recording failed to start: {e}"))),
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failures_read_as_the_panel_words() {
        assert_eq!(start_error(&SimError::CameraDenied("raw".into())), CAMERA_DENIED);
        assert_eq!(start_error(&SimError::DeviceBusy("raw".into())), DEVICE_BUSY);
        assert_eq!(start_error(&SimError::DeviceNotBooted), "device is not booted");
    }

    #[test]
    fn a_log_name_is_one_safe_component() {
        assert_eq!(log_name(&DeviceId("iosdev:00008130-0001".into())), "iosdev-00008130-0001");
    }
}
