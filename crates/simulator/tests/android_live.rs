//! Live: the scrcpy 4.1 protocol against a real Android device (an emulator
//! or a phone adb lists as `device`). `#[ignore]`d like the simulator live
//! tests, so a plain `cargo test` reports them as not run:
//!
//! ```sh
//! cargo test -p oximux-simulator --test android_live -- --ignored --nocapture
//! ```
//!
//! `OXIMUX_ANDROID_CAPTURE=<dir>` also writes one small config + key-frame
//! pair there (a real H.264 sample for the decoder's tests).

use std::time::{Duration, Instant};

use oximux_simulator::android::adb::{Adb, AdbState};
use oximux_simulator::android::scrcpy_control::{ControlMsg, KeyAction};
use oximux_simulator::android::scrcpy_server::{self, StreamOptions};
use oximux_simulator::android::scrcpy_video::{self, VideoEvent};
use oximux_simulator::android::{keycode, sdk};
use oximux_simulator::runner::SystemRunner;

const T: Duration = Duration::from_secs(20);

#[test]
#[ignore = "needs a running Android device; run with --ignored"]
fn scrcpy_streams_h264_and_takes_input() {
    let sdk = sdk::discover_here(None).expect("an Android SDK with adb");
    let runner = SystemRunner;
    let adb = Adb::new(&runner, &sdk.adb());
    let serial = adb
        .devices(T)
        .expect("adb devices")
        .into_iter()
        .find(|d| d.state == AdbState::Device)
        .expect("a running device")
        .serial;
    eprintln!("device {serial}");

    let cache = tempfile::tempdir().unwrap();
    let jar = scrcpy_server::ensure_jar(cache.path()).expect("the pinned jar");
    adb.push(&serial, &jar, scrcpy_server::DEVICE_JAR, T).expect("push");

    let capture = std::env::var_os("OXIMUX_ANDROID_CAPTURE");
    let opts = StreamOptions { max_size: if capture.is_some() { 160 } else { 1280 }, max_fps: 60 };
    let scid = scrcpy_server::new_scid();
    let port = adb.forward(&serial, &format!("localabstract:{}", scrcpy_server::socket_name(scid)), T).expect("forward");
    let started = Instant::now();
    let mut conn = scrcpy_server::start(&sdk.adb(), &serial, scid, opts, port).expect("server");
    eprintln!("connected to {:?} in {:?}", conn.device_name, started.elapsed());

    scrcpy_video::read_codec(&mut conn.video).expect("h264");
    let (mut session, mut config, mut key, mut frames) = (None, None, None, 0u32);
    let deadline = Instant::now() + Duration::from_secs(4);
    // Wake the screen so frames flow even if it was idle.
    scrcpy_server::send(&mut conn.control, &ControlMsg::Keycode { action: KeyAction::Down, keycode: keycode::HOME, repeat: 0, metastate: 0 }).unwrap();
    scrcpy_server::send(&mut conn.control, &ControlMsg::Keycode { action: KeyAction::Up, keycode: keycode::HOME, repeat: 0, metastate: 0 }).unwrap();
    while Instant::now() < deadline {
        match scrcpy_video::read_event(&mut conn.video).expect("packet") {
            VideoEvent::Session { width, height, .. } => session = Some((width, height)),
            VideoEvent::Media(p) if p.config => config = Some(p),
            VideoEvent::Media(p) => {
                if p.key_frame && key.is_none() {
                    key = Some(p.clone());
                }
                frames += 1;
            }
        }
    }
    eprintln!("session {session:?}, {frames} frames in 4 s, key frame {} bytes", key.as_ref().map_or(0, |k| k.data.len()));
    let (config, key) = (config.expect("a config packet"), key.expect("a key frame"));
    assert!(session.is_some(), "a session packet first");
    assert!(oximux_simulator::video::annexb::parameter_sets(&config.data).is_some(), "SPS and PPS");

    if let Some(dir) = capture {
        let dir = std::path::PathBuf::from(dir);
        std::fs::write(dir.join("config.h264"), &config.data).unwrap();
        std::fs::write(dir.join("key.h264"), &key.data).unwrap();
        eprintln!("captured {} + {} bytes, {:?}", config.data.len(), key.data.len(), session);
    }

    let _ = conn.server.kill();
    let _ = conn.server.wait();
    let _ = adb.forward_remove(&serial, conn.port, T);
}

/// The full path: a session decodes the live stream on the GPU while swipes
/// go in through the same `Command`s the panel sends. Prints fps and CPU.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs a running Android device; run with --ignored"]
fn a_session_decodes_while_input_flows() {
    use oximux_simulator::android::session::AndroidSession;
    use oximux_simulator::protocol::{Command, TouchPhase};
    use oximux_simulator::video::vt_decoder::is_paintable;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let sdk = sdk::discover_here(None).expect("an Android SDK with adb");
    let runner = SystemRunner;
    let adb = Adb::new(&runner, &sdk.adb());
    let serial = adb.devices(T).unwrap().into_iter().find(|d| d.state == AdbState::Device).expect("a running device").serial;
    let cache = tempfile::tempdir().unwrap();
    let jar = scrcpy_server::ensure_jar(cache.path()).unwrap();
    let id = oximux_simulator::android::Target::Serial(serial.clone()).id();
    let session = AndroidSession::start(&sdk.adb(), &jar, id, &serial, StreamOptions { max_size: 1280, max_fps: 60 }).expect("session");
    let woken = Arc::new(AtomicU32::new(0));
    let w = woken.clone();
    session.set_wake(move || {
        w.fetch_add(1, Ordering::Relaxed);
    });

    let cpu = || {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        t(usage.ru_utime) + t(usage.ru_stime)
    };
    std::thread::sleep(Duration::from_millis(800));
    let (cpu0, t0, seq0) = (cpu(), Instant::now(), session.latest_picture(0).map_or(0, |(s, _)| s));
    // Swipe up and down on the home screen (opens and closes the app list)
    // for four seconds: continuous motion to decode.
    let swipe = |from: f64, to: f64| {
        session.send(&Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: from, edge: 0 }).unwrap();
        for step in 1..=12 {
            let y = from + (to - from) * f64::from(step) / 12.0;
            session.send(&Command::Touch { phase: TouchPhase::Move, x: 0.5, y, edge: 0 }).unwrap();
            std::thread::sleep(Duration::from_millis(16));
        }
        session.send(&Command::Touch { phase: TouchPhase::End, x: 0.5, y: to, edge: 0 }).unwrap();
        std::thread::sleep(Duration::from_millis(500));
    };
    while t0.elapsed() < Duration::from_secs(4) {
        swipe(0.85, 0.3);
        swipe(0.3, 0.85);
    }
    let (secs, cpu_secs) = (t0.elapsed().as_secs_f64(), cpu() - cpu0);
    let (seq, picture) = session.latest_picture(0).expect("a decoded picture");
    let fps = (seq - seq0) as f64 / secs;
    eprintln!(
        "{:?}, portrait {:?}, {:.0} fps decoded, CPU {:.1}% of one core, {} wakes",
        picture,
        session.framebuffer_size(),
        fps,
        cpu_secs / secs * 100.0,
        woken.load(Ordering::Relaxed)
    );
    assert!(is_paintable(picture.buffer()));
    assert!(fps > 10.0, "frames flow while the screen moves");

    // A hidden panel pauses: nothing is decoded however the screen moves.
    session.pause().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let paused_at = session.latest_picture(0).map_or(0, |(s, _)| s);
    swipe(0.85, 0.3);
    swipe(0.3, 0.85);
    assert_eq!(session.latest_picture(0).map_or(0, |(s, _)| s), paused_at, "no decoding while paused");
    // Resuming asks for a key frame: a fresh picture within a second, even on
    // a still screen.
    let resumed = Instant::now();
    session.resume().unwrap();
    while session.latest_picture(paused_at).is_none() && resumed.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("a picture {:?} after resuming", resumed.elapsed());
    assert!(session.latest_picture(paused_at).is_some(), "a frame within 1 s of resuming");
    session.shutdown();
}

/// A recording stops by its own device pid, comes home as an MP4, and leaves
/// nothing behind on the device.
#[test]
#[ignore = "needs a running Android device; run with --ignored"]
fn a_recording_is_pulled_and_cleaned_up() {
    use oximux_simulator::record::Recording;

    let sdk = sdk::discover_here(None).expect("an Android SDK with adb");
    let runner = SystemRunner;
    let adb = Adb::new(&runner, &sdk.adb());
    let serial = adb.devices(T).unwrap().into_iter().find(|d| d.state == AdbState::Device).expect("a running device").serial;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.mp4");
    let id = oximux_simulator::android::Target::Serial(serial.clone()).id();
    let recording = Recording::start_android(&sdk.adb(), &serial, &id, &path, None).expect("recorder starts");
    std::thread::sleep(Duration::from_secs(3));
    let started = Instant::now();
    let saved = recording.stop(Duration::from_secs(5)).expect("a finished movie");
    let bytes = std::fs::read(&saved).unwrap();
    eprintln!("{} bytes in {:?}", bytes.len(), started.elapsed());
    assert_eq!(&bytes[4..8], b"ftyp", "an MP4");
    let left = adb.shell(&serial, &["ls", "/sdcard/"], T).unwrap();
    assert!(!left.contains("oximux-recording"), "nothing left on the device: {left}");
}
