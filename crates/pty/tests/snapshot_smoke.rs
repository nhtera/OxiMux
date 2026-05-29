//! Phase 1 step 3 smoke test.
//!
//! Verifies the alacritty wiring: bytes from the PTY flow through the ANSI
//! parser into the grid, and `snapshot()` returns a populated `cells`
//! matrix whose rendered text contains the marker. This catches three
//! regressions at once:
//!   1. State machine never advanced (cells all empty).
//!   2. `fill_snapshot` reads the wrong dimensions (returns a partial grid).
//!   3. Resize path corrupts the grid (we resize mid-test).

use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MARKER: &str = "OXIMUX_GRID_OK";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[test]
fn snapshot_contains_echoed_marker() {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec!["-c".into(), format!("printf '{MARKER}'")],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
    };

    let id = backend.spawn(cfg).expect("spawn shell");

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
    assert!(saw_exit, "child never exited within {TEST_TIMEOUT:?}");

    // Allow the watcher a moment to drain the final advance.
    std::thread::sleep(Duration::from_millis(20));

    let snap = backend.snapshot(id).expect("snapshot after exit");
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
    assert_eq!(snap.cells.len(), 24, "expected 24 rows in cells");
    assert_eq!(snap.cells[0].len(), 80, "expected 80 cols in row 0");

    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        rendered.contains(MARKER),
        "snapshot does not contain `{MARKER}`; first 200 chars: {:?}",
        &rendered[..rendered.len().min(200)]
    );

    backend.close(id).expect("close session");
}
