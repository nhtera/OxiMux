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
        scrollback: 5000,
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
                scrollback: 5000,
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
        scrollback: 5000,
    };
    let id = backend.spawn(cfg.clone()).expect("spawn live shell");
    let err = backend
        .promote_to_live(id, cfg)
        .expect_err("promote_to_live on live session must error");
    assert!(format!("{err:#}").contains("already live"));
    backend.close(id).expect("close");
}

/// F4.7: spawn a shell that prints an OSC 7 file:// URI, drain output,
/// and confirm `cwd_hint` returns the path the shell emitted. Catches
/// regressions in:
///   - watcher wiring the OSC 7 scanner alongside grid advancement,
///   - the Arc<Mutex<Option<PathBuf>>> threading,
///   - `cwd_hint` accessor on the backend.
#[test]
fn osc7_emission_populates_cwd_hint() {
    let mut backend = PortablePtyBackend::new();
    // Shell prints the OSC 7 sequence to stdout. `\033]7;file:///tmp/osc7-test\007`
    // is `ESC ] 7 ; file:///tmp/osc7-test BEL`. We add a final newline +
    // an OXIMUX_DONE marker so the test knows when the chunk landed.
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf '\\033]7;file:///tmp/osc7-test\\007OXIMUX_DONE\\n'".into(),
        ],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
    };
    let id = backend.spawn(cfg).expect("spawn shell");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut saw_done = false;
    let mut output_acc: Vec<u8> = Vec::new();
    while Instant::now() < deadline && !saw_done {
        for event in backend.drain_events() {
            if let TerminalEvent::Output { id: eid, bytes } = event
                && eid == id
            {
                output_acc.extend_from_slice(&bytes);
                if output_acc
                    .windows(b"OXIMUX_DONE".len())
                    .any(|w| w == b"OXIMUX_DONE")
                {
                    saw_done = true;
                }
            }
        }
        if !saw_done {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
    assert!(saw_done, "shell never produced OXIMUX_DONE marker");

    let hint = backend.cwd_hint(id);
    assert_eq!(
        hint,
        Some(PathBuf::from("/tmp/osc7-test")),
        "cwd_hint should reflect the OSC 7 file:// path the shell emitted"
    );

    backend.close(id).expect("close");
}

/// `cwd_hint` returns `None` for a session that has never seen an OSC 7
/// sequence. The caller is expected to fall back to libproc. Dormant
/// sessions also report `None` because they have no shell child.
#[test]
fn cwd_hint_is_none_before_first_osc7() {
    let mut backend = PortablePtyBackend::new();
    // Dormant: no PTY, no scanner, no shell — cwd_hint must be None.
    let id = backend.spawn_dormant(80, 24).expect("dormant");
    assert!(backend.cwd_hint(id).is_none());
    backend.close(id).expect("close");

    // Live session that never emits OSC 7: cwd_hint also None.
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec!["-c".into(), "echo no-osc7-here".into()],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
    };
    let id2 = backend.spawn(cfg).expect("spawn");
    // Drain output to ensure the watcher ran.
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut acc: Vec<u8> = Vec::new();
    while Instant::now() < deadline && !acc.windows(11).any(|w| w == b"no-osc7-here") {
        for event in backend.drain_events() {
            if let TerminalEvent::Output { bytes, .. } = event {
                acc.extend_from_slice(&bytes);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    assert!(backend.cwd_hint(id2).is_none());
    backend.close(id2).expect("close");
}
