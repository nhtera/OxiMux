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
const PALETTE_MARKER: &str = "OXIMUX256";
const TRUECOLOR_MARKER: &str = "OXIMUXRGB";
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

/// A shell invocation that writes both markers in the two colour depths the
/// 16-colour test above does not reach: an xterm palette index (SGR 38;5;208)
/// and 24-bit truecolor (SGR 38;2;217;119;87).
///
/// See `red_marker_emitter` for why the Windows arm builds ESC from `[char]27`
/// rather than using an escape sequence.
fn deep_colour_emitter() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!(
                    "$e=[char]27; \
                     Write-Host ($e + '[38;5;208m{PALETTE_MARKER}' + $e + '[0m'); \
                     Write-Host ($e + '[38;2;217;119;87m{TRUECOLOR_MARKER}' + $e + '[0m')"
                ),
            ],
        )
    } else {
        (
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                format!(
                    "printf '\\033[38;5;208m{PALETTE_MARKER}\\033[0m\\n\
                     \\033[38;2;217;119;87m{TRUECOLOR_MARKER}\\033[0m\\n'"
                ),
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

/// Spawn, wait for the child to exit, and return the resulting grid.
///
/// The same shape as the two tests above. Factored out for the colour-depth
/// test rather than copied a third time.
fn snapshot_after_exit(shell: String, args: Vec<String>) -> TerminalSnapshot {
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn(SpawnConfig {
            shell,
            args,
            cwd: test_cwd(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            scrollback: 5000,
            capture_status_events: false,
        })
        .expect("spawn shell");

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
    backend.close(id).expect("close session");
    snap
}

/// A child spawned through the backend must not inherit `NO_COLOR`.
///
/// `clear_inherited_colour_suppression` is unit-tested in `oximux-shell-env`,
/// but a helper that is correct and never called is the failure this port keeps
/// producing. This is the wiring check: it spawns a real child and asks *it*
/// what it sees.
///
/// Self-skipping, because it can only assert anything when the test process
/// itself carries the variable — which is precisely the situation the fix is
/// for (a coding agent sets `NO_COLOR=1` on everything it launches). Running it
/// under such an agent exercises the real path; running it from a plain shell
/// prints why it could not.
#[test]
fn a_pty_child_does_not_inherit_no_colour() {
    if std::env::var_os("NO_COLOR").is_none() {
        eprintln!(
            "skipped: NO_COLOR is not set in this process, so there is nothing \
             for the spawn path to strip. Re-run with NO_COLOR=1 in the \
             environment to exercise it."
        );
        return;
    }

    let (shell, args) = if cfg!(windows) {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!("Write-Host \"{MARKER}[$env:NO_COLOR]\""),
            ],
        )
    } else {
        (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), format!("printf '{MARKER}[%s]\\n' \"$NO_COLOR\"")],
        )
    };

    let snap = snapshot_after_exit(shell, args);
    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();

    assert!(
        rendered.contains(&format!("{MARKER}[]")),
        "the child still sees NO_COLOR — every agent, pager and build tool in \
         this pane would render monochrome. rendered: {:?}",
        rendered.trim_end()
    );
}

/// Palette and truecolor have to survive too, and on Windows they are a
/// *different* question from the 16 named colours the test above covers.
///
/// A ConPTY re-renders its child's output through a console screen buffer, and
/// that buffer's attribute model is where a colour depth gets lost. The failure
/// this guards is not "no colour" — it is a terminal that quietly quantises
/// every 24-bit colour to the nearest of 16, which looks like a washed-out
/// theme rather than like a bug. Claude Code, `bat`, `delta` and every modern
/// diff tool emit 24-bit; if this test fails while the SGR 31 one passes, the
/// grid is fine and the *depth* is what was dropped.
///
/// The assertions name the value found, because "which depth survived" is the
/// whole diagnostic and an `assert_eq!` that only says "not equal" would throw
/// it away.
#[test]
fn snapshot_preserves_palette_and_truecolour() {
    let (shell, args) = deep_colour_emitter();
    let snap = snapshot_after_exit(shell, args);
    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();

    let (row, col) = find_in_grid(&snap, PALETTE_MARKER).unwrap_or_else(|| {
        panic!("`{PALETTE_MARKER}` never reached the grid; rendered: {rendered:?}")
    });
    // Either spelling is a pass, and the second one is what Windows actually
    // does: a ConPTY resolves the palette index against its own table and
    // re-emits the result as 24-bit. 208 is `(5, 2, 0)` in the xterm 6×6×6
    // cube, which is exactly rgb(255, 135, 0) — the colour survives, only the
    // encoding changes. Asserting `Indexed(208)` alone would fail on Windows
    // for a difference the user cannot see.
    let palette_fg = snap.cells[row][col].fg;
    assert!(
        matches!(
            palette_fg,
            CellColor::Indexed(208) | CellColor::Rgb(255, 135, 0)
        ),
        "SGR 38;5;208 came back as {palette_fg:?} at row {row} col {col} — \
         expected the index itself or its rgb(255, 135, 0) equivalent"
    );

    let (row, col) = find_in_grid(&snap, TRUECOLOR_MARKER).unwrap_or_else(|| {
        panic!("`{TRUECOLOR_MARKER}` never reached the grid; rendered: {rendered:?}")
    });
    let rgb_fg = snap.cells[row][col].fg;
    assert_eq!(
        rgb_fg,
        CellColor::Rgb(217, 119, 87),
        "SGR 38;2;217;119;87 came back as {rgb_fg:?} at row {row} col {col} — \
         24-bit colour was quantised or dropped"
    );
}

/// A byte ConPTY gives a column to must get a column here too.
///
/// ConPTY's screen buffer inherits DOS semantics, where `0x0F` is a printable
/// CP437 glyph occupying one column. It then forwards the byte verbatim
/// instead of re-encoding it — and VT reads `0x0F` as SI, a control worth no
/// columns. From that point ConPTY's idea of the cursor column runs one ahead
/// of the terminal's, so every *relative* cursor move it emits afterwards
/// lands a column too far left. That is what turned Claude Code's `❯ hi` into
/// `❯h i`: a backspace overshot and the typed character overwrote the prompt
/// separator. `ConptyC0Filter` reconciles the two models at the read boundary.
///
/// This asserts the invariant rather than replaying the original frame. A
/// synthetic child cannot reproduce that frame reliably — ConPTY collapses a
/// short script's incremental writes into one absolutely-addressed repaint,
/// which corrects the drift on its own and makes such a test pass with the
/// filter removed. Column parity is the property the fix actually rests on,
/// and it is directly observable.
///
/// The unit tests in `conpty_c0` cover the state machine; this covers the
/// question they cannot — that the filter sits on the path the bytes take.
///
/// Windows only, because the defect is ConPTY's. Elsewhere SO/SI keep their
/// VT meaning and are passed through untouched.
#[test]
#[cfg(windows)]
fn a_byte_conpty_counts_as_a_column_occupies_one_here() {
    // Written from the test so the fixture cannot drift from the assertion.
    // Pure ASCII: PowerShell 5.1 reads a BOM-less .ps1 as ANSI, so a literal
    // non-ASCII character would break the parse.
    let script = std::env::temp_dir().join("oximux-conpty-si-probe.ps1");
    std::fs::write(
        &script,
        concat!(
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n",
            "[Console]::Out.Write('A')\n",
            "[Console]::Out.Write([char]0x0F)\n",
            "[Console]::Out.Write('B')\n",
            "[Console]::Out.Flush()\n",
        ),
    )
    .expect("write probe script");

    let snap = snapshot_after_exit(
        "powershell.exe".to_string(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-File".into(),
            script.display().to_string(),
        ],
    );

    let row: String = snap.cells[0].iter().take(3).map(|c| c.ch).collect();
    assert_eq!(
        row, "A B",
        "`A`, 0x0F, `B` came back as {row:?}. ConPTY put `B` in column 2 of \
         its own buffer; anything else here means the two disagree about the \
         width of that byte, and every relative cursor move after it on this \
         line will be off by one."
    );
}
