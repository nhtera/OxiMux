//! Phase 1 step 1-2 smoke test.
//!
//! Spawns `/bin/sh -c 'echo OXIMUX_HELLO'` through the portable-pty backend,
//! drains events until we see the marker in an `Output` chunk or the
//! deadline expires, then asserts both the marker AND a subsequent `Exit`
//! event. Catches three regressions cheaply:
//!   1. Reader thread never started (we'd see no events).
//!   2. Bounded channel deadlock (we'd time out before the marker).
//!   3. Exit detection broken (we'd see Output but never Exit).

use oximux_shell_env::test_support::{lines, run_script, test_cwd, test_shell};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent};
// Only the `#[cfg(unix)]` OSC 7 test below constructs a PathBuf.
#[cfg(unix)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MARKER: &str = "OXIMUX_HELLO";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// argv that makes the shell print a literal OSC 7 sequence.
///
/// Unix-only, and not for want of trying: `cmd.exe` has no way to emit a bare
/// ESC byte, and routing this one case through PowerShell would mean quoting an
/// escape through two parsers to test a code path that is platform-neutral once
/// the bytes exist. What IS Windows-specific here — decoding `file:///C:/...`
/// into an absolute path — is covered by unit tests in `oximux_pty::osc7` that
/// do run there.
#[cfg(unix)]
fn osc7_emitter() -> Vec<String> {
    vec![
        "-c".to_string(),
        "printf '\\033]7;file:///tmp/osc7-test\\007OXIMUX_DONE\\n'".to_string(),
    ]
}

#[test]
fn spawn_echo_drains_marker_and_exit() {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: test_shell(),
        args: run_script(&[&format!("echo {MARKER}")]),
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
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

#[test]
fn status_drain_does_not_consume_renderer_output_or_exit() {
    const STATUS_MARKER: &[u8] = b"STATUS_MARKER";
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn(SpawnConfig {
            shell: test_shell(),
            args: run_script(&["echo STATUS_MARKER", "exit 7"]),
            capture_status_events: true,
            ..SpawnConfig::default()
        })
        .expect("spawn status test shell");
    backend
        .subscribe_status_events(id)
        .expect("status registration is idempotent");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut status_bytes = Vec::new();
    let mut status_exit = None;
    while Instant::now() < deadline && status_exit.is_none() {
        for event in backend.drain_status_events_for(id) {
            match event {
                TerminalEvent::Output { bytes, .. } => status_bytes.extend(bytes),
                TerminalEvent::Exit { code, .. } => status_exit = Some(code),
                _ => panic!("status stream must contain only Output/Exit"),
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let mut renderer_bytes = Vec::new();
    let mut renderer_exit = None;
    while Instant::now() < deadline && renderer_exit.is_none() {
        for event in backend.drain_events_for(id) {
            match event {
                TerminalEvent::Output { bytes, .. } => renderer_bytes.extend(bytes),
                TerminalEvent::Exit { code, .. } => renderer_exit = Some(code),
                _ => {}
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    assert!(
        status_bytes
            .windows(STATUS_MARKER.len())
            .any(|bytes| bytes == STATUS_MARKER)
    );
    assert_eq!(status_exit, Some(Some(7)));
    assert!(
        renderer_bytes
            .windows(STATUS_MARKER.len())
            .any(|bytes| bytes == STATUS_MARKER),
        "status consumer stole renderer output"
    );
    assert_eq!(renderer_exit, Some(Some(7)));
    backend.unsubscribe_status_events(id);
    backend.close(id).expect("close status test shell");
}

// Generates 2 MB with `yes | head`, which cmd.exe has no counterpart for.
// The property under test — the status stream still delivers Exit after the
// renderer queue overflows — is platform-neutral ring-buffer behavior.
#[cfg(unix)]
#[test]
fn status_exit_survives_renderer_backpressure() {
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn(SpawnConfig {
            shell: test_shell(),
            args: vec![
                "-c".into(),
                "yes X | head -c 2097152; printf 'STATUS_TAIL\\n'; exit 7".into(),
            ],
            capture_status_events: true,
            ..SpawnConfig::default()
        })
        .expect("spawn high-output shell");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut status_exit = None;
    let mut status_bytes = Vec::new();
    while Instant::now() < deadline && status_exit.is_none() {
        for event in backend.drain_status_events_for(id) {
            match event {
                TerminalEvent::Output { bytes, .. } => status_bytes.extend(bytes),
                TerminalEvent::Exit { code, .. } => {
                    assert!(
                        status_bytes
                            .windows(b"STATUS_TAIL".len())
                            .any(|bytes| bytes == b"STATUS_TAIL"),
                        "status Exit overtook final PTY output"
                    );
                    status_exit = Some(code);
                }
                _ => {}
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    assert_eq!(
        status_exit,
        Some(Some(7)),
        "a full renderer channel must not stall status completion"
    );
    let _ = backend.drain_events_for(id);
    backend.unsubscribe_status_events(id);
    backend.close(id).expect("close high-output shell");
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
        .write(id, &lines(&["echo nope"]))
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
                shell: test_shell(),
                args: run_script(&["exit 0"]),
                cwd: test_cwd(),
                env: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback: 5000,
                capture_status_events: false,
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
        shell: test_shell(),
        args: run_script(&["exit 0"]),
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
    };
    let id = backend.spawn(cfg.clone()).expect("spawn live shell");
    let err = backend
        .promote_to_live(id, cfg)
        .expect_err("promote_to_live on live session must error");
    assert!(format!("{err:#}").contains("already live"));
    backend.close(id).expect("close");
}

/// Display-only seam: register a dormant session, stream bytes through
/// `write_output`, and verify they land in the grid emulator visible
/// via `snapshot`. The grid mirrors what an external producer (an
/// agent CLI / replayer) wrote without going through a PTY child.
#[test]
fn write_output_on_dormant_session_lands_in_grid() {
    const AGENT_MARKER: &str = "OXIMUX_AGENT_STREAM";
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn_dormant(80, 24)
        .expect("dormant session registers");

    let chunk = format!("{AGENT_MARKER}\r\n");
    backend
        .write_output(id, chunk.as_bytes())
        .expect("write_output on dormant session");

    let snap = backend.snapshot(id).expect("snapshot dormant session");
    let snapshot_text: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        snapshot_text.contains(AGENT_MARKER),
        "write_output bytes missing from dormant snapshot: {snapshot_text:?}"
    );

    backend.close(id).expect("close session");
}

/// Display-only invariant: `write_output` must refuse a session that
/// has a live PTY child, otherwise the producer would race the watcher
/// thread feeding the same parser concurrently and scramble cells.
#[test]
fn write_output_rejects_live_session() {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: test_shell(),
        args: run_script(&["exit 0"]),
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
    };
    let id = backend.spawn(cfg).expect("spawn live shell");
    let err = backend
        .write_output(id, b"agent bytes")
        .expect_err("write_output on live session must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("live PTY child") || msg.contains("write()"),
        "expected live/PTY hint in error message; got: {msg}"
    );
    backend.close(id).expect("close session");
}

/// F4.7: spawn a shell that prints an OSC 7 file:// URI, drain output,
/// and confirm `cwd_hint` returns the path the shell emitted. Catches
/// regressions in:
///   - watcher wiring the OSC 7 scanner alongside grid advancement,
///   - the Arc<Mutex<Option<PathBuf>>> threading,
///   - `cwd_hint` accessor on the backend.
// Needs a shell that can emit a raw ESC byte; see `osc7_emitter`.
#[cfg(unix)]
#[test]
fn osc7_emission_populates_cwd_hint() {
    let mut backend = PortablePtyBackend::new();
    // Shell prints the OSC 7 sequence to stdout. `\033]7;file:///tmp/osc7-test\007`
    // is `ESC ] 7 ; file:///tmp/osc7-test BEL`. We add a final newline +
    // an OXIMUX_DONE marker so the test knows when the chunk landed.
    let cfg = SpawnConfig {
        shell: test_shell(),
        args: osc7_emitter(),
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
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
        shell: test_shell(),
        args: run_script(&["echo no-osc7-here"]),
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
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
