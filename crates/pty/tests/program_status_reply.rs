//! The in-process backend answers a program's OSC 7501 feature query itself,
//! exactly once, with no view mounted — and queues its status reports for an
//! agent's poll loop.
//!
//! Unix-only: the child is a POSIX shell that reads its own input with `stty`
//! timeouts, which `cmd.exe` cannot do. The gate and parser that decide what is
//! answered are platform-neutral and unit-tested in `oximux_pty::program_status`.
#![cfg(unix)]

use std::time::{Duration, Instant};

use oximux_pty::{
    PortablePtyBackend, ProgramState, SpawnConfig, TerminalBackend, TerminalEvent,
};

/// The reply, as `od -An -tx1` prints it.
const REPLY_HEX: &str = "1b 5d 37 35 30 31 3b 3f 1b 5c";

/// Ask twice back to back, then report; print every input byte received
/// within a second of quiet as hex, then `DONE`.
fn script() -> Vec<String> {
    let q = r"\033]7501;?\007";
    vec![
        "-c".into(),
        format!(
            "stty raw -echo min 0 time 10; printf '{q}{q}\\033]7501;state=working:app=t\\007'; \
             dd bs=1 count=64 2>/dev/null | od -An -tx1 | tr -d '\\n'; printf '\\r\\nDONE\\r\\n'"
        ),
    ]
}

fn cfg(capture_status_events: bool) -> SpawnConfig {
    SpawnConfig {
        shell: "/bin/sh".into(),
        args: script(),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        cols: 200,
        rows: 24,
        scrollback: 100,
        capture_status_events,
    }
}

/// Drain until the script prints `DONE`; returns the renderer's output text
/// and the program statuses the renderer received.
fn run_until_done_with_statuses(backend: &mut PortablePtyBackend) -> (String, Vec<Option<ProgramState>>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    let mut statuses = Vec::new();
    while Instant::now() < deadline {
        for ev in backend.drain_events() {
            match ev {
                TerminalEvent::Output { bytes, .. } => out.extend_from_slice(&bytes),
                TerminalEvent::ProgramStatus { root, .. } => statuses.push(root.map(|r| r.state)),
                _ => {}
            }
        }
        if String::from_utf8_lossy(&out).contains("DONE") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (String::from_utf8_lossy(&out).into_owned(), statuses)
}

fn run_until_done(backend: &mut PortablePtyBackend) -> String {
    run_until_done_with_statuses(backend).0
}

fn assert_one_reply(output: &str) {
    assert!(output.contains("DONE"), "script never finished: {output:?}");
    // `od` spacing differs between platforms; compare word by word.
    let words = output.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(words.matches(REPLY_HEX).count(), 1, "exactly one reply expected: {output:?}");
}

#[test]
fn a_live_session_answers_once_and_queues_its_status() {
    let mut backend = PortablePtyBackend::new();
    let id = backend.spawn(cfg(true)).expect("spawn");
    let (output, rendered) = run_until_done_with_statuses(&mut backend);
    assert_one_reply(&output);
    // The renderer gets it too, through the lossless latest-status slot.
    assert_eq!(rendered, vec![Some(ProgramState::Working)]);
    let statuses: Vec<_> = backend
        .drain_status_events_for(id)
        .into_iter()
        .filter_map(|e| match e {
            TerminalEvent::ProgramStatus { root, handshake, .. } => Some((root.map(|r| r.state), handshake)),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, vec![(Some(ProgramState::Working), true)]);
    let _ = backend.close(id);
}

#[test]
fn a_promoted_dormant_session_answers_once() {
    let mut backend = PortablePtyBackend::new();
    let id = backend.spawn_dormant(200, 24).expect("dormant");
    backend.promote_to_live(id, cfg(false)).expect("promote");
    assert_one_reply(&run_until_done(&mut backend));
    let _ = backend.close(id);
}
