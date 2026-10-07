//! The capture helper (`OxiMux Device Capture.app`) through the very spawn
//! OxiMux uses for it — responsible for itself — with no iPhone and no
//! permission prompt: its version, a refused argument, and its
//! `--conformance` stand-in for a phone (handshake, commands, recordings,
//! each failure).
//!
//! The app is `$OXIMUX_DEVICE_CAPTURE` (a local build, e.g. the fork's
//! `oximux/scripts/dev-app.sh`), else the release `scripts/fetch-sim-helper.sh`
//! stages in `target/bundle-tools/`. When neither exists the tests skip
//! loudly — except under CI, which fetches it first.
#![cfg(target_os = "macos")]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oximux_simulator::availability::CAPTURE_OVERRIDE;
use oximux_simulator::helper::{self, HelperKind, HelperOptions};
use oximux_simulator::ios_device::DeviceSession;
use oximux_simulator::protocol::{Command, TouchPhase};
use oximux_simulator::record::{FINALIZE_GRACE, Recording};
use oximux_simulator::session::HelperSession;
use oximux_simulator::{DeviceId, Orientation, SimError};

fn capture_app() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CAPTURE_OVERRIDE) {
        return Some(PathBuf::from(path));
    }
    let staged = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/bundle-tools/OxiMux Device Capture.app/Contents/MacOS/oximux-device-capture");
    staged.exists().then_some(staged)
}

macro_rules! require_capture_app {
    () => {
        match capture_app() {
            Some(path) => path,
            None => {
                let why = format!("no capture app (run scripts/fetch-sim-helper.sh or set {CAPTURE_OVERRIDE})");
                assert!(std::env::var_os("CI").is_none(), "{why}");
                eprintln!("SKIPPED: {why}");
                return;
            }
        }
    };
}

#[test]
fn the_capture_app_runs_responsible_for_itself() {
    let app = require_capture_app!();
    let mut spawned = helper::spawn_kind(HelperKind::DeviceCapture, &app, &["--version".into()], None).expect("spawns");
    let mut out = String::new();
    spawned.stdout.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("oximux-device-capture "), "{out}");
    assert!(spawned.child.wait().unwrap().success());
}

#[test]
fn a_bad_device_id_is_refused_before_the_camera_is_asked_for() {
    let app = require_capture_app!();
    let opts = HelperOptions { kind: HelperKind::DeviceCapture, ..HelperOptions::default() };
    // Not hex: the helper says hello, then refuses its arguments.
    let err = HelperSession::start(&app, &DeviceId("iosdev:not-a-udid".into()), &opts).err().expect("refused");
    assert!(matches!(&err, SimError::HelperFailed(why) if why.contains("--device")), "{err}");
}

/// The capture app's `--conformance` stand-in, as OxiMux starts a phone:
/// spawned responsible for itself, through the handshake.
fn conformance_session(app: &Path, extra: &[&str]) -> oximux_simulator::Result<HelperSession> {
    let opts = HelperOptions { kind: HelperKind::DeviceCapture, ..HelperOptions::default() };
    let mut args = vec!["--conformance".to_owned()];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    HelperSession::start_with_args(app, &args, &DeviceId("iosdev:00008130-0001".into()), &opts)
}

/// Waits (≤ 5 s) for `ready` to be followed by the screen's size.
fn wait_for_size(session: &HelperSession) -> Option<(u32, u32)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(size) = session.framebuffer_size() {
            return Some(size);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn a_capture_session_starts_and_stays_portrait() {
    let app = require_capture_app!();
    let session = conformance_session(&app, &[]).expect("starts");
    assert_eq!(wait_for_size(&session), Some((1290, 2796)), "the displayed size, as is");
    assert_eq!(session.orientation(), Orientation::Portrait);
    let iphone = DeviceSession::new(session);
    // Answered by the helper…
    assert!(iphone.request(&Command::Ping, Duration::from_secs(5)).is_ok());
    // …refused here, before it.
    let touch = Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.5, edge: 0 };
    assert!(matches!(iphone.send(&touch), Err(SimError::Unsupported(_))));
}

#[test]
fn an_iphone_recording_moves_home_when_stopped() {
    let app = require_capture_app!();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home").join("Recording.mov");
    std::fs::create_dir_all(home.parent().unwrap()).unwrap();
    let iphone = DeviceSession::new(conformance_session(&app, &[]).expect("starts"));
    let recording = Recording::start_capture(iphone, &dir.path().join("staging"), &home).expect("records");
    assert_eq!(recording.stop(FINALIZE_GRACE).expect("finished"), home);
    assert!(std::fs::read(&home).unwrap().starts_with(b"oximux conformance movie"));
    assert!(!dir.path().join("staging").join("Recording.mov").exists(), "moved, not copied");
}

#[test]
fn an_iphone_recording_survives_its_helper_ending_first() {
    let app = require_capture_app!();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("Recording.mov");
    let iphone = DeviceSession::new(conformance_session(&app, &[]).expect("starts"));
    let video = iphone.video().clone();
    let recording = Recording::start_capture(iphone, &dir.path().join("staging"), &home).expect("records");
    // The stream goes first (an unplug, a closed session): the helper
    // finishes the movie on its way out and says so.
    video.shutdown();
    assert_eq!(recording.stop(FINALIZE_GRACE).expect("kept"), home);
    assert!(home.exists());
}

#[test]
fn each_capture_failure_maps_to_its_error() {
    let app = require_capture_app!();
    let fails = |reason: &str| conformance_session(&app, &["--fatal", reason]).err().expect("fails");
    assert!(matches!(fails("camera_denied"), SimError::CameraDenied(_)));
    assert!(matches!(fails("device_busy"), SimError::DeviceBusy(_)));
    assert!(matches!(fails("device_not_connected"), SimError::DeviceNotBooted));
}
