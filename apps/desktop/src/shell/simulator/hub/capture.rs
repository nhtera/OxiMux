//! The hub's device actions beyond streaming: hardware buttons, rotation,
//! screenshots and screen recordings. Results the user should hear about go
//! out as [`HubEvent::Notice`]; the window turns them into toasts.
//!
//! Screenshots and recordings go through `simctl`, not the helper, so they
//! work at full resolution and whether or not a stream is running (a parked
//! device has no helper).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{ClipboardItem, Context, Image, ImageFormat};
use oximux_simulator::record::{self, FINALIZE_GRACE, MAX_LENGTH, Recording};
use oximux_simulator::{Button, DeviceId, simctl};

use super::{HubEvent, SIMCTL_TIMEOUT, SimulatorHub};

/// How loud a notice is (the window picks the toast style).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Success,
    Error,
}

/// What a capture is, for its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureKind {
    Screenshot,
    Recording,
}

/// `Simulator Screenshot - iPhone 17 Pro - 2026-09-26 at 02.51.07.png`, the
/// naming Simulator.app uses, in `dir`. `stamp` is the local time already
/// formatted as `yyyy-MM-dd at HH.mm.ss`. Path separators and colons in the
/// device name are replaced, so the name is always one path component.
pub(crate) fn capture_path(dir: &Path, kind: CaptureKind, device: &str, stamp: &str) -> PathBuf {
    let device: String = device.chars().map(|c| if matches!(c, '/' | ':' | '\\') { '-' } else { c }).collect();
    let (what, ext) = match kind {
        CaptureKind::Screenshot => ("Simulator Screenshot", "png"),
        CaptureKind::Recording => ("Simulator Screen Recording", "mov"),
    };
    dir.join(format!("{what} - {} - {stamp}.{ext}", device.trim()))
}

/// Where captures go: the Desktop (like Simulator.app), else Downloads,
/// else the app's own `simulator/captures`. The Desktop and Downloads are
/// privacy-protected on macOS: the first write asks the user, and a denial
/// moves captures to the app's folder (the toast names the file).
///
/// Probes by writing, which can wait on that permission prompt: call it
/// off the UI thread.
pub(crate) fn capture_dir() -> PathBuf {
    let writable = |dir: &Path| {
        let probe = dir.join(".oximux-capture-probe");
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    };
    [dirs::desktop_dir(), dirs::download_dir()].into_iter().flatten().find(|d| d.is_dir() && writable(d)).unwrap_or_else(
        || {
            let dir = super::simulator_dir().join("captures");
            let _ = std::fs::create_dir_all(&dir);
            dir
        },
    )
}

pub(crate) fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d at %H.%M.%S").to_string()
}

/// The button that goes home on this device: the home gesture on Face ID
/// devices and iPads, the hardware button where there is one (a swipe up on
/// an iPhone SE opens Control Center instead). Upstream's `home` relaunches
/// SpringBoard, which does not leave the app switcher; the gesture does.
pub(crate) fn home_button(device_name: &str) -> Button {
    if device_name.contains("iPhone SE") { Button::Home } else { Button::SwipeHome }
}

impl SimulatorHub {
    fn device_name(&self, udid: &DeviceId) -> String {
        self.devices.iter().find(|d| &d.udid == udid).map_or_else(|| "Simulator".into(), |d| d.name.clone())
    }

    fn notice(&self, udid: &DeviceId, kind: NoticeKind, text: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(HubEvent::Notice(udid.clone(), kind, text.into()));
    }

    /// Go home. False when there is no live stream to send it through.
    pub fn home(&self, udid: &DeviceId) -> bool {
        self.send_button(udid, home_button(&self.device_name(udid)))
    }

    /// Press the side (lock) button. False without a live stream.
    pub fn lock(&self, udid: &DeviceId) -> bool {
        self.send_button(udid, Button::Lock)
    }

    fn send_button(&self, udid: &DeviceId, button: Button) -> bool {
        let Some(session) = self.session(udid) else { return false };
        session.send(&oximux_simulator::protocol::Command::Button { name: button }).is_ok()
    }

    /// Save a full-resolution screenshot of `udid` to the Desktop and put it
    /// on the clipboard.
    pub fn screenshot(&self, udid: &DeviceId, cx: &mut Context<Self>) {
        if !self.xcode_ok() {
            return;
        }
        let (runner, target, session) = (self.runner.clone(), udid.clone(), self.session(udid));
        let (device, stamp) = (self.device_name(udid), stamp());
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    // The helper's is rotated like the stream; without one
                    // (parked, starting) simctl still has the device.
                    let png = match session.map(|s| s.screenshot_png(SIMCTL_TIMEOUT)) {
                        Some(Ok(png)) => png,
                        _ => simctl::screenshot_png(runner.as_ref(), target.as_str(), SIMCTL_TIMEOUT)
                            .map_err(|e| e.to_string())?,
                    };
                    let path = capture_path(&capture_dir(), CaptureKind::Screenshot, &device, &stamp);
                    std::fs::write(&path, &png).map_err(|e| format!("could not write {}: {e}", path.display()))?;
                    Ok::<_, String>((png, path))
                })
                .await;
            let _ = this.update(cx, |hub, cx| match saved {
                Ok((png, path)) => {
                    cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    hub.notice(&udid, NoticeKind::Success, format!("Screenshot saved and copied: {name}"), cx);
                }
                Err(e) => hub.notice(&udid, NoticeKind::Error, format!("Screenshot failed: {e}"), cx),
            });
        })
        .detach();
    }

    /// When `udid`'s recording started, if one is running.
    pub fn recording_since(&self, udid: &DeviceId) -> Option<Instant> {
        self.recordings.get(udid).map(|r| r.started)
    }

    /// Start recording `udid`, or stop the recording in progress.
    pub fn toggle_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if self.recordings.contains_key(udid) {
            self.stop_recording(udid, cx);
        } else {
            self.start_recording(udid, cx);
        }
    }

    fn start_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if !self.xcode_ok() || !self.recording_starts.insert(udid.clone()) {
            return; // no Xcode, or a start is already in flight
        }
        let (runner, ledger, cached) = (self.runner.clone(), self.ledger.clone(), self.simctl.clone());
        let (device, stamp) = (self.device_name(udid), stamp());
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let target = udid.clone();
            let started = cx
                .background_executor()
                .spawn(async move {
                    let simctl = match cached {
                        Some(path) => path,
                        None => record::simctl_path(runner.as_ref(), SIMCTL_TIMEOUT).map_err(|e| e.to_string())?,
                    };
                    let path = capture_path(&capture_dir(), CaptureKind::Recording, &device, &stamp);
                    let recording = Recording::start(&simctl, &target, &path, ledger).map_err(|e| e.to_string())?;
                    Ok::<_, String>((simctl, recording))
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                // Cancelled meanwhile (detached, switched, stopped): finalize
                // what started rather than record invisibly.
                let wanted = hub.recording_starts.remove(&udid);
                match started {
                    Ok((simctl, recording)) if !wanted => {
                        hub.simctl = Some(simctl);
                        cx.background_executor().spawn(async move { drop(recording.stop(FINALIZE_GRACE)) }).detach();
                    }
                    Ok((simctl, recording)) => {
                        hub.simctl = Some(simctl);
                        let since = recording.started;
                        hub.recordings.insert(udid.clone(), recording);
                        hub.auto_stop(udid.clone(), since, cx);
                        cx.emit(HubEvent::Changed(udid));
                    }
                    Err(e) => hub.notice(&udid, NoticeKind::Error, format!("Recording failed to start: {e}"), cx),
                }
            });
        })
        .detach();
    }

    /// Stop the recording at [`MAX_LENGTH`], if it is still the same one.
    fn auto_stop(&self, udid: DeviceId, since: Instant, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MAX_LENGTH).await;
            let _ = this.update(cx, |hub, cx| {
                if hub.recording_since(&udid) == Some(since) {
                    hub.stop_recording(&udid, cx);
                }
            });
        })
        .detach();
    }

    /// Stop `udid`'s recording (or cancel one still starting) and finalize
    /// the movie in the background.
    pub fn stop_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.stop_recording_then(udid, cx, |_, _| {});
    }

    /// [`Self::stop_recording`], then `then` once the movie is finalized (at
    /// once when nothing was recording).
    pub fn stop_recording_then(
        &mut self,
        udid: &DeviceId,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        self.recording_starts.remove(udid);
        let Some(recording) = self.recordings.remove(udid) else { return then(self, cx) };
        cx.emit(HubEvent::Changed(udid.clone()));
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let done = cx.background_executor().spawn(async move { recording.stop(FINALIZE_GRACE) }).await;
            let _ = this.update(cx, |hub, cx| {
                hub.report_recording(&udid, done, cx);
                then(hub, cx);
            });
        })
        .detach();
    }

    /// Finalize the recordings of devices nobody is attached to any more.
    pub(super) fn stop_unattached_recordings(&mut self, cx: &mut Context<Self>) {
        let attached = self.registry.attached_devices();
        let orphans: Vec<DeviceId> = self
            .recordings
            .keys()
            .chain(self.recording_starts.iter())
            .filter(|u| !attached.contains(u))
            .cloned()
            .collect();
        for udid in orphans {
            self.stop_recording(&udid, cx);
        }
    }

    /// Shut `udid` down after its recording (if any) is finalized, so the
    /// movie is not cut off by the device going away.
    pub fn shutdown_after_recording(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let target = udid.clone();
        self.stop_recording_then(udid, cx, move |hub, cx| hub.shutdown_device(&target, cx));
    }

    fn report_recording(&self, udid: &DeviceId, done: oximux_simulator::Result<PathBuf>, cx: &mut Context<Self>) {
        match done {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.notice(udid, NoticeKind::Success, format!("Recording saved: {name}"), cx);
            }
            Err(e) => self.notice(udid, NoticeKind::Error, format!("Recording did not finish cleanly: {e}"), cx),
        }
    }

    /// Recordings whose `simctl` exited on its own (the device shut down)
    /// are finalized and reported. Called from the tick.
    pub(super) fn reap_recordings(&mut self, cx: &mut Context<Self>) {
        let ended: Vec<DeviceId> =
            self.recordings.iter_mut().filter_map(|(u, r)| (!r.is_running()).then(|| u.clone())).collect();
        for udid in ended {
            self.stop_recording(&udid, cx);
        }
    }

    /// App quit: stop every recording now and wait for the movies, all in
    /// parallel, so quitting waits at most one [`FINALIZE_GRACE`]. A start
    /// still in flight is left to the child ledger (reaped next launch).
    pub(super) fn stop_recordings_blocking(&mut self) {
        let stopping: Vec<_> = self
            .recordings
            .drain()
            .map(|(udid, recording)| {
                std::thread::spawn(move || {
                    if let Err(e) = recording.stop(FINALIZE_GRACE) {
                        tracing::warn!(%udid, "recording not finalized at quit: {e}");
                    }
                })
            })
            .collect();
        for thread in stopping {
            let _ = thread.join();
        }
    }

    pub(super) fn xcode_ok(&self) -> bool {
        self.watch_gate().xcode_ok
    }
}

/// Serializes pastes so two quick ⌘V land in order.
pub(super) type PasteLock = Arc<std::sync::Mutex<()>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_names_follow_simulator_app_and_stay_one_component() {
        let dir = Path::new("/Users/x/Desktop");
        assert_eq!(
            capture_path(dir, CaptureKind::Screenshot, "iPhone 17 Pro", "2026-09-26 at 02.51.07"),
            dir.join("Simulator Screenshot - iPhone 17 Pro - 2026-09-26 at 02.51.07.png")
        );
        let movie = capture_path(dir, CaptureKind::Recording, "a/b:c", "2026-09-26 at 02.51.07");
        assert_eq!(movie.parent(), Some(dir));
        assert!(movie.to_string_lossy().ends_with("Simulator Screen Recording - a-b-c - 2026-09-26 at 02.51.07.mov"));
    }

    #[test]
    fn home_is_the_gesture_except_on_a_home_button_iphone() {
        assert_eq!(home_button("iPhone 17 Pro"), Button::SwipeHome);
        assert_eq!(home_button("iPad Air 11-inch (M3)"), Button::SwipeHome);
        assert_eq!(home_button("iPhone SE (3rd generation)"), Button::Home);
    }
}
