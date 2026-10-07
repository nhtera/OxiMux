//! The capture helper (`OxiMux Device Capture.app`) through the very spawn
//! OxiMux uses for it — responsible for itself — with no iPhone: its version,
//! and its handshake up to refusing arguments (which comes before any camera
//! request, so no permission prompt shows).
//!
//! The app is `$OXIMUX_DEVICE_CAPTURE` (a local build, e.g. the fork's
//! `oximux/scripts/dev-app.sh`), else the release staged in
//! `target/bundle-tools/`. When neither exists the tests skip loudly.
#![cfg(target_os = "macos")]

use std::io::Read as _;
use std::path::PathBuf;

use oximux_simulator::availability::CAPTURE_OVERRIDE;
use oximux_simulator::helper::{self, HelperKind, HelperOptions};
use oximux_simulator::session::HelperSession;
use oximux_simulator::{DeviceId, SimError};

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
                eprintln!("SKIPPED: no capture app (set {CAPTURE_OVERRIDE}, or stage the release in target/bundle-tools)");
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
