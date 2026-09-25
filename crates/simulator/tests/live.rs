//! End-to-end against a real booted simulator. Opt-in, because it needs
//! Xcode, a booted device, and the helper binary:
//!
//! ```sh
//! OXIMUX_SIM_UDID=<booted udid> OXIMUX_SIM_HELPER=/path/to/oximux-sim-helper \
//!   cargo test -p oximux-simulator --test live -- --ignored
//! ```
//!
//! `#[ignore]`d so a plain `cargo test` reports it as not run instead of
//! passing it vacuously.
//!
//! Without `OXIMUX_SIM_UDID` the first booted iPhone is used. The test leaves
//! the device in portrait and does not boot or shut anything down.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use oximux_simulator::helper::HelperOptions;
use oximux_simulator::protocol::{Command, TouchPhase};
use oximux_simulator::session::{SessionEvent, StreamSession};
use oximux_simulator::{DeviceId, Orientation};

fn live_target() -> (PathBuf, DeviceId) {
    let helper = std::env::var_os("OXIMUX_SIM_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bundle-tools/oximux-sim-helper"));
    let udid = std::env::var("OXIMUX_SIM_UDID").ok().or_else(first_booted_iphone).expect("no booted iPhone simulator");
    (helper, DeviceId(udid))
}

fn first_booted_iphone() -> Option<String> {
    let out = std::process::Command::new("xcrun").args(["simctl", "list", "devices", "booted", "-j"]).output().ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    json["devices"].as_object()?.values().filter_map(|v| v.as_array()).flatten().find_map(|d| {
        let name = d["name"].as_str()?;
        name.contains("iPhone").then(|| d["udid"].as_str().map(str::to_owned)).flatten()
    })
}

fn wait_for_frame(session: &StreamSession, seen: u64, timeout: Duration) -> (u64, u32, u32) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((seq, frame)) = session.latest_frame(seen) {
            return (seq, frame.width, frame.height);
        }
        assert!(Instant::now() < deadline, "no frame within {timeout:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs Xcode and a booted simulator; run with --ignored"]
fn streams_rotates_screenshots_and_takes_input() {
    let (helper, udid) = live_target();
    let session = StreamSession::start(&helper, &udid, &HelperOptions::default()).expect("helper starts");
    let events = session.take_events().unwrap();
    assert!(session.hello().xcode.is_some());

    // Portrait frames at Half scale.
    let (seq, w, h) = wait_for_frame(&session, 0, Duration::from_secs(10));
    assert!(h > w, "portrait frame expected, got {w}x{h}");
    let (fb_w, fb_h) = session.framebuffer_size().expect("size precedes frames");
    assert!(fb_h > fb_w);

    // Requests do not wait on each other, and replies route by id.
    let png = session.screenshot_png(Duration::from_secs(10)).expect("screenshot");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let ax = session.request(&Command::AxFrontmost, Duration::from_secs(10)).expect("ax_frontmost");
    assert!(ax.get("bundleId").is_some(), "{ax}");

    // Rotation: the event fires and frames turn landscape.
    session.configure(None, None, Some(Orientation::LandscapeRight), Duration::from_secs(10)).expect("rotate");
    assert_eq!(session.orientation(), Orientation::LandscapeRight);
    session.resume().unwrap(); // force a fresh frame even on a static screen
    let mut seen = seq;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, w, h) = wait_for_frame(&session, seen, Duration::from_secs(10));
        seen = s;
        if w > h {
            break;
        }
        assert!(Instant::now() < deadline, "frames never turned landscape");
    }
    session.configure(None, None, Some(Orientation::Portrait), Duration::from_secs(10)).expect("rotate back");

    // Input is fire-and-forget; a tap on the status bar is harmless.
    for phase in [TouchPhase::Begin, TouchPhase::End] {
        session.send(&Command::Touch { phase, x: 0.5, y: 0.01, edge: 0 }).unwrap();
    }
    session.pause().unwrap();
    session.resume().unwrap();

    session.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(SessionEvent::Exited { code, .. }) => {
                assert_eq!(code, Some(0));
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("helper did not exit after shutdown: {e}"),
        }
    }
}
