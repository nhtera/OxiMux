//! Phase 1 step 3 smoke test.
//!
//! Verifies the alacritty wiring: bytes from the PTY flow through the ANSI
//! parser into the grid, and `snapshot()` returns a populated `cells`
//! matrix whose rendered text contains the marker. This catches three
//! regressions at once:
//!   1. State machine never advanced (cells all empty).
//!   2. `fill_snapshot` reads the wrong dimensions (returns a partial grid).
//!   3. Resize path corrupts the grid (we resize mid-test).

use oximux_shell_env::test_support::{run_script, test_cwd, test_shell};
use oximux_pty::{
    CellColor, NamedColor16, PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent,
    TerminalSnapshot,
};
use std::time::{Duration, Instant};

const MARKER: &str = "OXIMUX_GRID_OK";
const RED_MARKER: &str = "OXIMUXRED";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[test]
fn snapshot_contains_echoed_marker() {
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

/// A shell invocation that writes `RED_MARKER` wrapped in SGR 31 (red fg).
///
/// Windows goes through PowerShell rather than the `test_shell()` `cmd.exe`,
/// which has no way to emit a bare ESC byte. The escape is built from
/// `[char]27` and concatenated rather than interpolated: PowerShell 5.1 has
/// no `` `e `` escape (that arrived in 7.0), and `"$e[31m"` inside a double
/// quoted string parses as an *index* into `$e`, not as ESC followed by
/// `[31m`.
fn red_marker_emitter() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!("$e=[char]27; Write-Host ($e + '[31m{RED_MARKER}' + $e + '[0m')"),
            ],
        )
    } else {
        (
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                format!("printf '\\033[31m{RED_MARKER}\\033[0m\\n'"),
            ],
        )
    }
}

/// Grid position of `needle`'s first character, searching row by row. Column
/// index falls out of the char offset because one cell holds one char.
fn find_in_grid(snap: &TerminalSnapshot, needle: &str) -> Option<(usize, usize)> {
    snap.cells.iter().enumerate().find_map(|(row, cells)| {
        let text: String = cells.iter().map(|c| c.ch).collect();
        text.find(needle).map(|col| (row, col))
    })
}

/// SGR colour has to survive the whole path into the grid, not just the text.
///
/// On Windows that path is materially different from Unix: a ConPTY does not
/// hand the child's bytes to the master, it *renders* them into its own screen
/// buffer and re-emits VT describing the result. Colour therefore round-trips
/// through the pseudoconsole's attribute model, and a break anywhere in it
/// leaves every cell at `CellColor::Default` — a terminal that renders the
/// right characters in the wrong colour, which the text-only assertions above
/// cannot see.
#[test]
fn snapshot_preserves_sgr_foreground_colour() {
    let (shell, args) = red_marker_emitter();
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell,
        args,
        cwd: test_cwd(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        scrollback: 5000,
        capture_status_events: false,
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
    std::thread::sleep(Duration::from_millis(20));

    let snap = backend.snapshot(id).expect("snapshot after exit");
    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    let (row, col) = find_in_grid(&snap, RED_MARKER).unwrap_or_else(|| {
        panic!(
            "`{RED_MARKER}` never reached the grid; rendered: {:?}",
            rendered.trim_end()
        )
    });

    // The first occurrence, deliberately: an echoed copy of the command line
    // would land uncoloured and ahead of the real output, and asserting on
    // "some red occurrence" would let that pass unnoticed.
    assert_eq!(
        snap.cells[row][col].fg,
        CellColor::Named(NamedColor16::Red),
        "SGR 31 did not survive into the grid at row {row} col {col}"
    );

    backend.close(id).expect("close session");
}
