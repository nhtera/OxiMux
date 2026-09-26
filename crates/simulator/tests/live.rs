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
use oximux_simulator::protocol::{Command, StreamFormat, TouchPhase};
use oximux_simulator::session::{FrameData, HelperSession, SessionEvent};
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

fn wait_for_frame(session: &HelperSession, seen: u64, timeout: Duration) -> (u64, u32, u32) {
    let (seq, frame) = wait_for_data(session, seen, timeout);
    let (w, h) = frame.size();
    (seq, w, h)
}

fn wait_for_data(session: &HelperSession, seen: u64, timeout: Duration) -> (u64, FrameData) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = session.latest_frame(seen) {
            return found;
        }
        assert!(Instant::now() < deadline, "no frame within {timeout:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs Xcode and a booted simulator; run with --ignored"]
fn streams_rotates_screenshots_and_takes_input() {
    let (helper, udid) = live_target();
    let session = HelperSession::start(&helper, &udid, &HelperOptions::default()).expect("helper starts");
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

/// The same stream in H.264: pictures come decoded (IOSurface-backed), turn
/// with the device, come back fast after a resume, and the encoding switches
/// live both ways.
#[test]
#[ignore = "needs Xcode and a booted simulator; run with --ignored"]
fn streams_h264_and_switches_encoding() {
    let (helper, udid) = live_target();
    let opts = HelperOptions { format: StreamFormat::Avcc, ..HelperOptions::default() };
    let session = HelperSession::start(&helper, &udid, &opts).expect("helper starts");
    assert!(session.supports_avcc(), "helper {} has no H.264", session.hello().version);

    let (mut seen, frame) = wait_for_data(&session, 0, Duration::from_secs(10));
    assert!(matches!(frame, FrameData::Picture(_)), "expected a decoded picture");
    let (w, h) = frame.size();
    assert!(h > w, "portrait picture expected, got {w}x{h}");

    session.configure(None, None, Some(Orientation::LandscapeLeft), Duration::from_secs(10)).expect("rotate");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, w, h) = wait_for_frame(&session, seen, Duration::from_secs(10));
        seen = s;
        if w > h {
            break;
        }
        assert!(Instant::now() < deadline, "pictures never turned landscape");
    }
    session.configure(None, None, Some(Orientation::Portrait), Duration::from_secs(10)).expect("rotate back");

    session.pause().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    seen = session.latest_frame(0).map_or(seen, |(s, _)| s);
    let resumed = Instant::now();
    session.resume().unwrap();
    let (s, _) = wait_for_data(&session, seen, Duration::from_secs(5));
    let took = resumed.elapsed();
    eprintln!("H.264 resume → picture in {took:?}");
    assert!(took < Duration::from_secs(1), "resume took {took:?}");
    seen = s;

    session.set_format(StreamFormat::Jpeg, Duration::from_secs(5)).expect("to JPEG");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, frame) = wait_for_data(&session, seen, Duration::from_secs(10));
        seen = s;
        if matches!(frame, FrameData::Jpeg(_)) {
            break;
        }
        assert!(Instant::now() < deadline, "never switched to JPEG");
    }
    session.set_format(StreamFormat::Avcc, Duration::from_secs(5)).expect("back to H.264");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, frame) = wait_for_data(&session, seen, Duration::from_secs(10));
        seen = s;
        if matches!(frame, FrameData::Picture(_)) {
            break;
        }
        assert!(Instant::now() < deadline, "never switched back to H.264");
    }
    session.shutdown();
}
