//! Our protocol code against the helper binary that actually ships.
//!
//! `oximux-sim-helper --conformance` needs no simulator: it emits one of every
//! message shape, then echoes each command as it parsed it. So these tests pin
//! the Rust encoder/decoder to the real Swift parser, not to a copy of it.
//!
//! The binary is `$OXIMUX_SIM_HELPER` when set (a locally built fork), else the
//! pinned release `scripts/fetch-sim-helper.sh` caches in
//! `target/bundle-tools/`. When neither exists the tests skip LOUDLY rather
//! than fail, so a cold checkout still runs `cargo test` — except under CI
//! (`CI` set), where a missing helper is a failure: CI fetches it first.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::Duration;

use oximux_simulator::child_ledger::{Kind, Ledger};
use oximux_simulator::helper::{self, HelperOptions};
use oximux_simulator::protocol::{
    self, Command, Event, FatalReason, Frame, KeyPhase, Outbound, StreamFormat, TouchPhase, Video, VideoTag,
};
use oximux_simulator::session::{SessionEvent, HelperSession};
use oximux_simulator::{Button, DeviceId, Orientation};
use serde_json::{Value, json};

fn helper_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OXIMUX_SIM_HELPER") {
        return Some(PathBuf::from(path));
    }
    let cached = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bundle-tools/oximux-sim-helper");
    cached.exists().then_some(cached)
}

macro_rules! require_helper {
    () => {
        match helper_binary() {
            Some(path) => path,
            None => {
                let why = "no oximux-sim-helper (run scripts/fetch-sim-helper.sh or set OXIMUX_SIM_HELPER)";
                assert!(std::env::var_os("CI").is_none(), "{why}");
                eprintln!("SKIPPED: {why}");
                return;
            }
        }
    };
}

/// Run `--conformance`, send `commands` (each with its index as id), close
/// stdin, and return everything the helper wrote.
fn conformance_run(path: &std::path::Path, commands: &[Vec<u8>]) -> Vec<Outbound> {
    use std::io::Write as _;
    let mut spawned = helper::spawn(path, &["--conformance".into()], None).expect("spawn helper");
    for bytes in commands {
        spawned.stdin.write_all(bytes).unwrap();
    }
    drop(spawned.stdin);
    let mut out = Vec::new();
    while let Some(message) = protocol::read_message(&mut spawned.stdout).expect("well-formed output") {
        out.push(message);
    }
    let status = spawned.child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "helper exits 0 on stdin EOF");
    out
}

/// The protocol version the helper at `path` announces. Tests expect the
/// messages of that version, so the pinned (older) release and a locally
/// built newer fork both pass.
fn helper_proto(path: &std::path::Path) -> u32 {
    match conformance_run(path, &[]).first() {
        Some(Outbound::Event(Event::Hello { proto, .. })) => *proto,
        other => panic!("first message must be hello, got {other:?}"),
    }
}

#[test]
fn opening_sequence_decodes_to_every_message_shape() {
    let path = require_helper!();
    let out = conformance_run(&path, &[]);
    let v2 = helper_proto(&path) >= protocol::AVCC_MIN_VERSION;
    let mut expected = vec![
        Outbound::Frame(Frame { width: 3, height: 2, jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9] }),
        Outbound::Event(Event::Ready { udid: "conformance".into(), pid: 0, orientation: Some(Orientation::Portrait) }),
        Outbound::Event(Event::Size { width: 1206, height: 2622 }),
        Outbound::Event(Event::Orientation(Orientation::LandscapeRight)),
        Outbound::Event(Event::Response { id: 1, result: Ok(json!({"pong": true})) }),
        Outbound::Event(Event::Response { id: 2, result: Ok(json!([{"AXLabel": "raw"}])) }),
        Outbound::Event(Event::Response { id: 3, result: Err("example failure".into()) }),
        Outbound::Event(Event::Error { message: "example error".into() }),
        Outbound::Event(Event::Fatal { reason: FatalReason::FrameworkLoadFailed, message: "example fatal".into() }),
        Outbound::Event(Event::ConformanceReady),
    ];
    if v2 {
        let video = Video { width: 3, height: 2, tag: VideoTag::Description, data: vec![0x01, 0x64, 0x00, 0x1F] };
        expected.insert(1, Outbound::Video(video));
        let error_at = expected.iter().position(|m| matches!(m, Outbound::Event(Event::Error { .. }))).unwrap();
        let fallback = Event::Format { format: Some(StreamFormat::Jpeg), message: "example fallback".into() };
        expected.insert(error_at + 1, Outbound::Event(fallback));
    }
    match &out[0] {
        Outbound::Event(Event::Hello { proto, xcode, .. }) => {
            assert!(
                (protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION).contains(proto),
                "helper speaks protocol {proto}, outside what this build drives"
            );
            assert_eq!(xcode.as_deref(), Some("conformance"));
        }
        other => panic!("first message must be hello, got {other:?}"),
    }
    assert_eq!(&out[1..], &expected[..]);
}

#[test]
fn every_command_we_encode_parses_as_intended() {
    let path = require_helper!();
    // (what we send, what the helper must have understood)
    let mut cases: Vec<(Command, Value)> = vec![
        (Command::Ping, json!({"cmd": "ping"})),
        (Command::Touch { phase: TouchPhase::Begin, x: 0.25, y: 0.75, edge: 0 },
         json!({"cmd": "touch", "phase": "begin", "x": 0.25, "y": 0.75, "edge": 0})),
        (Command::Touch { phase: TouchPhase::End, x: 0.5, y: 1.0, edge: 3 },
         // Swift's JSONSerialization writes a whole double as an integer.
         json!({"cmd": "touch", "phase": "end", "x": 0.5, "y": 1, "edge": 3})),
        (Command::Multitouch { phase: TouchPhase::Move, x1: 0.1, y1: 0.2, x2: 0.3, y2: 0.4 },
         json!({"cmd": "multitouch", "phase": "move", "x1": 0.1, "y1": 0.2, "x2": 0.3, "y2": 0.4})),
        (Command::Scroll { dx: 0.0, dy: -12.5, x: Some(0.5), y: None },
         json!({"cmd": "scroll", "dx": 0, "dy": -12.5, "x": 0.5})),
        (Command::Key { phase: KeyPhase::Down, usage: 0xE3 }, json!({"cmd": "key", "phase": "down", "usage": 227})),
        (Command::Key { phase: KeyPhase::Up, usage: 0x19 }, json!({"cmd": "key", "phase": "up", "usage": 25})),
        (Command::Button { name: Button::Home }, json!({"cmd": "button", "name": "home"})),
        (Command::Button { name: Button::SideButton }, json!({"cmd": "button", "name": "side_button"})),
        (Command::Button { name: Button::SwipeHome }, json!({"cmd": "button", "name": "swipe_home"})),
        (Command::Configure { scale: Some(0.5), fps: Some(30.0), orientation: Some(Orientation::LandscapeLeft), format: None },
         json!({"cmd": "configure", "scale": 0.5, "fps": 30, "orientation": 3})),
        (Command::Configure { scale: None, fps: None, orientation: None, format: None }, json!({"cmd": "configure"})),
        (Command::Pause, json!({"cmd": "pause"})),
        (Command::Resume, json!({"cmd": "resume"})),
        (Command::Screenshot, json!({"cmd": "screenshot"})),
        (Command::AxDescribe, json!({"cmd": "ax_describe"})),
        (Command::AxFrontmost, json!({"cmd": "ax_frontmost"})),
        (Command::MemoryWarning, json!({"cmd": "memory_warning"})),
    ];
    // A protocol-1 helper rejects `format` (it never gets one: `set_format`
    // checks the version first).
    if helper_proto(&path) >= protocol::AVCC_MIN_VERSION {
        for format in [StreamFormat::Avcc, StreamFormat::Jpeg] {
            let command = Command::Configure { scale: None, fps: None, orientation: None, format: Some(format) };
            cases.push((command, json!({"cmd": "configure", "format": format.as_str()})));
        }
    }
    let encoded: Vec<Vec<u8>> =
        cases.iter().enumerate().map(|(i, (c, _))| protocol::encode_command(c, Some(i as u64 + 100))).collect();
    let parsed: Vec<(Option<u64>, Result<Value, String>)> = conformance_run(&path, &encoded)
        .into_iter()
        .filter_map(|m| match m {
            Outbound::Event(Event::Parsed { id, command }) => Some((id, command)),
            _ => None,
        })
        .collect();
    assert_eq!(parsed.len(), cases.len());
    for (i, ((id, got), (command, want))) in parsed.into_iter().zip(&cases).enumerate() {
        assert_eq!(id, Some(i as u64 + 100), "{command:?}");
        assert_eq!(got.as_ref(), Ok(want), "{command:?}");
    }
}

#[test]
fn helper_rejects_and_clamps_as_documented() {
    let path = require_helper!();
    let raw = |v: Value| {
        let body = serde_json::to_vec(&v).unwrap();
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend(body);
        out
    };
    let out = conformance_run(&path, &[
        raw(json!({"cmd": "touch", "phase": "begin", "x": 7, "y": -1})),
        raw(json!({"cmd": "configure", "orientation": 9})),
        raw(json!({"cmd": "button", "name": "volume_up"})),
        raw(json!({"cmd": "nope"})),
    ]);
    let parsed: Vec<_> = out
        .into_iter()
        .filter_map(|m| match m {
            Outbound::Event(Event::Parsed { command, .. }) => Some(command),
            _ => None,
        })
        .collect();
    assert_eq!(parsed[0], Ok(json!({"cmd": "touch", "phase": "begin", "x": 1, "y": 0, "edge": 0})));
    assert!(parsed[1..].iter().all(Result::is_err), "{parsed:?}");
}

/// The helper must exit when OxiMux lets go of it, even while other children
/// we spawned are still alive: none of them may hold its stdin open.
#[test]
fn helper_exits_when_the_session_is_dropped_despite_other_children() {
    let path = require_helper!();
    let dir = tempfile::tempdir().unwrap();
    let ledger = std::sync::Arc::new(Ledger::open(dir.path().join("children.json")).unwrap());
    let opts = HelperOptions { ledger: Some(ledger.clone()), ..HelperOptions::default() };
    let session =
        HelperSession::start_with_args(&path, &["--conformance".into()], &DeviceId("conformance".into()), &opts)
            .expect("conformance helper completes the handshake");
    let events = session.take_events().unwrap();
    let pid = session.pid();
    // Recorded while alive, with the path we launched it by (what reap_stale matches).
    let entries = ledger.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].pid, entries[0].kind, &entries[0].exe), (pid, Kind::Helper, &path));
    // Another child, spawned while the helper's stdin pipe exists.
    let mut bystander = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    drop(session);
    // Skip the conformance events still queued ahead of the exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let exited = loop {
        match events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(e @ SessionEvent::Exited { .. }) => break Ok(e),
            Ok(_) => continue,
            Err(e) => break Err(e),
        }
    };
    let _ = bystander.kill();
    let _ = bystander.wait();
    assert!(
        matches!(exited, Ok(SessionEvent::Exited { code: Some(0), .. })),
        "helper {pid} did not exit on stdin EOF: {exited:?}"
    );
    assert!(ledger.entries().unwrap().is_empty(), "a reaped helper leaves the ledger");
}
