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

/// F3.4 slice 2: spawn a dormant session, prefill scrollback bytes,
/// snapshot the grid → the prefilled cells must be visible. Then
/// promote-to-live + drain events → the live shell's stdout follows the
/// prefilled grid. Catches regressions in:
///   - `spawn_dormant`: session registered without a child.
///   - `prefill_grid` on a dormant session: bytes land in the grid emulator.
///   - `promote_to_live`: shell spawns + watcher arms, grid stays populated.
///   - The PTY-bound trait methods correctly reject dormant sessions
///     (`write` should error before promote_to_live).
#[test]
fn spawn_dormant_prefill_then_promote_to_live() {
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn_dormant(80, 24)
        .expect("dormant session registers");

    // Pre-promote: write must reject (no PTY child yet).
    let write_err = backend
        .write(id, b"echo nope\n")
        .expect_err("write on dormant session must error");
    let msg = format!("{write_err:#}");
    assert!(
        msg.contains("dormant") || msg.contains("promote_to_live"),
        "expected dormant/promote_to_live in error message; got: {msg}"
    );

    // Prefill with an ANSI marker — restorer feeds these bytes into the
    // grid emulator so the user sees prior scrollback BEFORE the shell
    // produces fresh output.
    const PREFILL_MARKER: &str = "OXIMUX_RESTORE_BANNER";
    let prefill = format!("{PREFILL_MARKER}\r\n");
    backend
        .prefill_grid(id, prefill.as_bytes())
        .expect("prefill_grid on dormant session");

    let snap = backend.snapshot(id).expect("snapshot dormant session");
    let snapshot_text: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        snapshot_text.contains(PREFILL_MARKER),
        "prefilled marker missing from dormant snapshot: {snapshot_text:?}"
    );

    // Promote to live → a real shell child now drives the grid. Marker
    // from prefill must survive the promotion.
    backend
        .promote_to_live(
            id,
            SpawnConfig {
                shell: "/bin/sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                cwd: PathBuf::from("/"),
                env: Vec::new(),
                cols: 80,
                rows: 24,
            },
        )
        .expect("promote_to_live on dormant session");

    // Drain to give the watcher a chance to reap the child + emit Exit.
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut saw_exit = false;
    while Instant::now() < deadline && !saw_exit {
        for event in backend.drain_events() {
            if let TerminalEvent::Exit { id: eid, .. } = event
                && eid == id
            {
                saw_exit = true;
            }
        }
        if !saw_exit {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    // Snapshot still shows the prefilled marker — prefilled scrollback
    // survived the promotion (it doesn't get wiped on shell spawn).
    let post_snap = backend.snapshot(id).expect("snapshot after promote");
    let post_text: String = post_snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        post_text.contains(PREFILL_MARKER),
        "prefilled marker lost after promote_to_live: {post_text:?}"
    );
    assert!(saw_exit, "no Exit event from `exit 0` shell after promote");

    backend.close(id).expect("close session");
}

/// Reject `promote_to_live` on a session that's already live. The
/// guard prevents accidental double-spawn from a confused caller.
#[test]
fn promote_to_live_rejects_already_live_session() {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 0".into()],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        cols: 80,
        rows: 24,
    };
    let id = backend.spawn(cfg.clone()).expect("spawn live shell");
    let err = backend
        .promote_to_live(id, cfg)
        .expect_err("promote_to_live on live session must error");
    assert!(format!("{err:#}").contains("already live"));
    backend.close(id).expect("close");
}
