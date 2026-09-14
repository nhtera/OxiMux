//! Command-mark gutter badges survive a real `clear(1)` without resurrecting.
//!
//! The badge a finished prompt draws in the terminal's left padding is anchored
//! to an absolute history line (`history_size + cursor_row` when the OSC 133 `A`
//! fired). `clear(1)` sends `ESC[3J`, which drops the scrollback to zero — every
//! line recorded before it then counts from a new origin, and any old mark whose
//! number lands inside the fresh viewport repaints its badge over unrelated
//! output. That is the defect this pins, end to end: a real shell, real OSC 133
//! marks, real `clear`, and the exact mapping the renderer applies.

#![cfg(unix)]

use oximux_pty::{
    CommandMarkKind, PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent,
};
use oximux_shell_env::test_support::{run_script, test_cwd, test_shell};
use std::time::{Duration, Instant};

const DONE: &str = "OXIMUX_MARKS_DONE";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const ROWS: u16 = 10;

/// What the renderer keeps: prompt marks by absolute history line, with the
/// exit code attached when the command finishes. Only finished marks badge.
#[derive(Default)]
struct Badges {
    marks: Vec<(u64, Option<i32>)>,
}

impl Badges {
    fn apply(&mut self, event: &TerminalEvent) {
        match event {
            TerminalEvent::CommandMark {
                kind: CommandMarkKind::PromptStart,
                line,
                ..
            } => self.marks.push((*line, None)),
            TerminalEvent::CommandMark {
                kind: CommandMarkKind::CommandEnd,
                exit,
                ..
            } => {
                if let Some(last) = self.marks.last_mut() {
                    last.1 = *exit;
                }
            }
            // The fix: a scrollback that shrank unanchors every line above.
            TerminalEvent::ScrollbackReset { .. } => self.marks.clear(),
            _ => {}
        }
    }

    /// Screen rows the gutter would paint, using the renderer's formula.
    fn rows(&self, history_len: usize, display_offset: usize, rows: usize) -> Vec<i64> {
        let base = history_len as i64 - display_offset as i64;
        self.marks
            .iter()
            .filter(|(_, exit)| exit.is_some())
            .map(|(line, _)| *line as i64 - base)
            .filter(|row| (0..rows as i64).contains(row))
            .collect()
    }
}

// Two commands early in the session take marks at low absolute lines (history
// is still empty). Filler then builds real scrollback behind them, and `clear`
// wipes it. Without the reset those two stale lines address rows of the FRESH
// screen and badge whatever output now sits there; with it, only the prompt
// redrawn after the wipe carries a badge.
#[test]
fn clear_does_not_resurrect_stale_command_badges() {
    let mut backend = PortablePtyBackend::new();
    let id = backend
        .spawn(SpawnConfig {
            shell: test_shell(),
            args: run_script(&[
                // Two finished commands, marked the way the shell integration
                // marks them (prompt-start, output, command-end + exit).
                r"printf '\033]133;A\007'",
                "echo one",
                r"printf '\033]133;D;0\007'",
                r"printf '\033]133;A\007'",
                "echo two",
                r"printf '\033]133;D;0\007'",
                // Enough output to scroll the marked rows into history.
                "i=0; while [ $i -lt 30 ]; do echo filler$i; i=$((i+1)); done",
                // The wipe. `clear` is the real thing, terminfo and all.
                "clear",
                // The prompt the shell redraws afterwards, also finished.
                r"printf '\033]133;A\007'",
                "echo after-clear",
                r"printf '\033]133;D;0\007'",
                &format!("echo {DONE}"),
            ]),
            cwd: test_cwd(),
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            cols: 80,
            rows: ROWS,
            scrollback: 5000,
            capture_status_events: false,
        })
        .expect("spawn marked shell");

    let mut badges = Badges::default();
    let mut saw_reset = false;
    let mut prompt_marks = 0usize;
    let mut output = Vec::new();
    let mut done = false;

    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline && !done {
        for event in backend.drain_events() {
            if event.session_id() != id {
                continue;
            }
            match &event {
                TerminalEvent::Output { bytes, .. } => output.extend_from_slice(bytes),
                TerminalEvent::ScrollbackReset { .. } => saw_reset = true,
                TerminalEvent::CommandMark {
                    kind: CommandMarkKind::PromptStart,
                    ..
                } => prompt_marks += 1,
                _ => {}
            }
            badges.apply(&event);
        }
        if output.windows(DONE.len()).any(|w| w == DONE.as_bytes()) {
            done = true;
        } else {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    let snapshot = backend.snapshot(id).expect("snapshot the cleared grid");
    backend.close(id).expect("close session");

    let transcript = String::from_utf8_lossy(&output).to_string();
    assert!(done, "shell never finished the script; got: {transcript:?}");
    assert_eq!(
        prompt_marks, 3,
        "precondition: all three prompts were marked; got: {transcript:?}"
    );
    assert!(
        saw_reset,
        "`clear` wiped the scrollback but no reset was reported; got: {transcript:?}"
    );

    // Row 0 is where the post-clear prompt actually sits. Anything else is a
    // stale mark painting a badge on unrelated output.
    let rows = badges.rows(
        snapshot.history_len,
        snapshot.display_offset,
        snapshot.rows as usize,
    );
    assert_eq!(
        rows,
        vec![0],
        "only the prompt redrawn after `clear` may badge (history_len={}, offset={})",
        snapshot.history_len,
        snapshot.display_offset
    );
}

