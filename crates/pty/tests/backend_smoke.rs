//! Phase 1 step 1-2 smoke test.
//!
//! Spawns `/bin/sh -c 'echo OXIMUX_HELLO'` through the portable-pty backend,
//! drains events until we see the marker in an `Output` chunk or the
//! deadline expires, then asserts both the marker AND a subsequent `Exit`
//! event. Catches three regressions cheaply:
//!   1. Reader thread never started (we'd see no events).
//!   2. Bounded channel deadlock (we'd time out before the marker).
//!   3. Exit detection broken (we'd see Output but never Exit).

use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MARKER: &str = "OXIMUX_HELLO";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[test]
fn spawn_echo_drains_marker_and_exit() {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec!["-c".into(), format!("echo {MARKER}")],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        cols: 80,
        rows: 24,
    };

    let id = backend.spawn(cfg).expect("spawn shell");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut saw_marker = false;
    let mut saw_exit = false;
    let mut output_acc: Vec<u8> = Vec::new();

    while Instant::now() < deadline && !(saw_marker && saw_exit) {
        for event in backend.drain_events() {
            match event {
                TerminalEvent::Output { id: eid, bytes } if eid == id => {
                    output_acc.extend_from_slice(&bytes);
                    if output_acc
                        .windows(MARKER.len())
                        .any(|w| w == MARKER.as_bytes())
                    {
                        saw_marker = true;
                    }
                }
                TerminalEvent::Exit { id: eid, .. } if eid == id => {
                    saw_exit = true;
                }
                _ => {}
            }
        }
        if !(saw_marker && saw_exit) {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    backend.close(id).expect("close session");

    let preview = String::from_utf8_lossy(&output_acc);
    assert!(
        saw_marker,
        "did not see `{MARKER}` in output within {TEST_TIMEOUT:?}; got: {preview:?}"
    );
    assert!(
        saw_exit,
        "did not see Exit event within {TEST_TIMEOUT:?}; got: {preview:?}"
    );
}
