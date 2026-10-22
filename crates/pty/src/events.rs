//! Lifecycle events surfaced by a `TerminalBackend`.
//!
//! Backends accumulate these internally (bounded by the channel capacity)
//! and the UI drains them once per frame via `drain_events`.

use std::path::PathBuf;

use crate::backend::TerminalSessionId;
use crate::program_status::ProgramRecord;

/// Shell-integration command-mark phases (OSC 133 / 633). The renderer uses
/// `PromptStart` to anchor an exit-code badge on the prompt row and
/// `CommandEnd` to attach the exit code to the most recent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMarkKind {
    /// OSC 133;A — a fresh prompt is about to render.
    PromptStart,
    /// OSC 133;B — end of prompt / start of the typed command line.
    CommandStart,
    /// OSC 133;C — command output begins (pre-exec).
    OutputStart,
    /// OSC 133;D[;exit] — the command finished; `exit` carries the code.
    CommandEnd,
}

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// Raw bytes from the PTY master. Step 3 will route these into the
    /// alacritty state machine; step 1-2 surfaces them directly so the
    /// smoke test can assert byte-level echo.
    Output {
        id: TerminalSessionId,
        bytes: Vec<u8>,
    },
    /// Child process exited. `code` is `Some` when we got a status; `None`
    /// on signal or detach.
    Exit {
        id: TerminalSessionId,
        code: Option<i32>,
    },
    /// The session died with the daemon that hosted it (a crash or a restart),
    /// not on its own. A new daemon is already up when this arrives, so the
    /// owner can bring the session back rather than show it as exited — and
    /// must not treat it as the program's own exit (no auto-closed tab, no
    /// failed agent).
    DaemonLost { id: TerminalSessionId },
    /// The PTY was resized. Echoed back so consumers can confirm the size
    /// the backend actually applied (may differ from requested by 1 cell
    /// on some platforms).
    Resize {
        id: TerminalSessionId,
        cols: u16,
        rows: u16,
    },
    /// OSC 2 title change. Forwarded raw; the UI decides how to display it.
    TitleChange {
        id: TerminalSessionId,
        title: String,
    },
    /// Terminal bell (BEL, 0x07) emitted by the child. Surfaced so the UI
    /// can raise an attention signal (pane ring / tab badge / OS banner)
    /// on the owning pane — a curses app or shell `\a` means "look here".
    Bell { id: TerminalSessionId },
    /// OSC 7 working-directory change. Backends that track cwd (the
    /// in-process PTY) fold this into their cwd cache; others ignore it.
    CwdChanged {
        id: TerminalSessionId,
        path: PathBuf,
    },
    /// OSC 133/633 shell-integration command mark. `line` is the absolute
    /// history line (history_size + cursor row) the mark was anchored at, so
    /// the renderer can place a gutter badge that scrolls with the content.
    CommandMark {
        id: TerminalSessionId,
        kind: CommandMarkKind,
        exit: Option<i32>,
        line: u64,
    },
    /// The scrollback that absolute history lines are counted from is gone:
    /// `CSI 3J` (erase saved lines — what `clear(1)` and the app's own Clear
    /// affordance send), `ESC c` (RIS, from `reset(1)`), or a resize whose
    /// reflow unwrapped rows back onto the screen. Every line recorded before
    /// it now counts from a new origin, so consumers holding them — the
    /// command-mark gutter badges — must drop what they have.
    ///
    /// Ordered against `CommandMark` as the bytes were, so the prompt the
    /// shell redraws once `clear` returns keeps its badge. Note this is NOT
    /// raised for a bare `CSI 2J` (zsh's Ctrl-L): alacritty scrolls those rows
    /// into history rather than discarding them, so the marks stay anchored to
    /// content that still exists, one scroll up.
    ScrollbackReset { id: TerminalSessionId },
    /// OSC 9;4 progress report. `state`: 0 clear, 1 set, 2 error, 3
    /// indeterminate, 4 warning. `value` is a 0..=100 percentage.
    Progress {
        id: TerminalSessionId,
        state: u8,
        value: u8,
    },
    /// OSC 52 clipboard-set: the child asked to put `text` on the system
    /// clipboard. The UI gates + writes it (alacritty pre-decodes base64).
    Clipboard {
        id: TerminalSessionId,
        text: String,
    },
    /// Bytes the emulator must write back to the PTY in response to a query
    /// (device status / attributes / cursor position, OSC color reply). The
    /// app drains these and calls `write` so in-process and relay backends
    /// reply through the same path.
    PtyReply {
        id: TerminalSessionId,
        bytes: Vec<u8>,
    },
    /// OSC 7501: the program's root status record changed. `None` means no
    /// program is reporting any more — it cleared its status, the terminal was
    /// reset, or a new shell prompt began (the program that reported is gone,
    /// even if its final `done` record is kept).
    ///
    /// `handshake` is whether a feature query was seen in this stream before
    /// the report. A program only reports once answered, so it is evidence the
    /// report came from the program that asked — but not proof (the query can
    /// fall out of a replay ring), so consumers combine it with what they know
    /// about the process.
    ProgramStatus {
        id: TerminalSessionId,
        root: Option<ProgramRecord>,
        handshake: bool,
    },
}

impl TerminalEvent {
    /// Session this event is addressed to. Used by backends that
    /// multiplex many panes onto one event source (e.g., the relay
    /// backend) to route a drained event back to the owning view.
    pub fn session_id(&self) -> TerminalSessionId {
        match self {
            TerminalEvent::Output { id, .. }
            | TerminalEvent::Exit { id, .. }
            | TerminalEvent::DaemonLost { id }
            | TerminalEvent::Resize { id, .. }
            | TerminalEvent::TitleChange { id, .. }
            | TerminalEvent::Bell { id }
            | TerminalEvent::CwdChanged { id, .. }
            | TerminalEvent::CommandMark { id, .. }
            | TerminalEvent::ScrollbackReset { id }
            | TerminalEvent::Progress { id, .. }
            | TerminalEvent::Clipboard { id, .. }
            | TerminalEvent::PtyReply { id, .. }
            | TerminalEvent::ProgramStatus { id, .. } => *id,
        }
    }
}

/// Most events a status-only subscriber (an agent poll loop) keeps queued.
pub const STATUS_EVENT_CAPACITY: usize = 256;

/// Whether a status-only subscriber wants `event` at all.
pub fn is_status_event(event: &TerminalEvent) -> bool {
    matches!(
        event,
        TerminalEvent::Output { .. } | TerminalEvent::Exit { .. } | TerminalEvent::ProgramStatus { .. }
    )
}

/// Queue `event` for a status-only subscriber, bounded at
/// [`STATUS_EVENT_CAPACITY`]. Under pressure the oldest `Output` goes first, and
/// an `Output` is dropped rather than evict anything else: losing a chunk costs
/// the regex fallback one scan, losing an `Exit` or a status change costs the
/// truth. Only the latest program status is worth keeping — each one replaces
/// the whole root record — so a new one supersedes any still queued, which also
/// keeps a report flood from growing the queue.
pub fn push_bounded_status(queue: &mut std::collections::VecDeque<TerminalEvent>, event: TerminalEvent) {
    if matches!(event, TerminalEvent::ProgramStatus { .. })
        && let Some(index) = queue
            .iter()
            .position(|queued| matches!(queued, TerminalEvent::ProgramStatus { .. }))
    {
        queue.remove(index);
    }
    if queue.len() >= STATUS_EVENT_CAPACITY {
        if let Some(index) = queue
            .iter()
            .position(|queued| matches!(queued, TerminalEvent::Output { .. }))
        {
            queue.remove(index);
        } else if matches!(event, TerminalEvent::Output { .. }) {
            return;
        }
    }
    queue.push_back(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    // `TerminalEvent` is a pure data enum — the compiler guarantees
    // variant payload shape. The one behavior worth exercising is byte
    // fidelity through Clone on a non-trivial Output payload, since that
    // is the hot path drained per frame by the renderer.
    #[test]
    fn output_clone_preserves_bytes_at_payload_size() {
        let payload: Vec<u8> = (0..=255).cycle().take(8 * 1024).collect();
        let ev = TerminalEvent::Output {
            id: TerminalSessionId(7),
            bytes: payload.clone(),
        };
        let cloned = ev.clone();
        match cloned {
            TerminalEvent::Output { id, bytes } => {
                assert_eq!(id, TerminalSessionId(7));
                assert_eq!(bytes, payload);
            }
            _ => panic!("clone changed variant"),
        }
    }

    fn status(state: &str) -> TerminalEvent {
        let report = crate::program_status::parse_report(state.as_bytes()).unwrap();
        let mut store = crate::program_status::ProgramStatusStore::new();
        store.apply(report);
        TerminalEvent::ProgramStatus {
            id: TerminalSessionId(1),
            root: store.root().cloned(),
            handshake: true,
        }
    }

    #[test]
    fn a_status_flood_keeps_only_the_latest_and_survives_output_pressure() {
        let mut queue = std::collections::VecDeque::new();
        let id = TerminalSessionId(1);
        push_bounded_status(&mut queue, status("state=working"));
        for _ in 0..STATUS_EVENT_CAPACITY * 2 {
            push_bounded_status(&mut queue, TerminalEvent::Output { id, bytes: vec![b'x'] });
            push_bounded_status(&mut queue, status("state=blocked"));
        }
        push_bounded_status(&mut queue, status("state=idle"));
        let statuses: Vec<_> = queue
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::ProgramStatus { root, .. } => root.as_ref().map(|r| r.state),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![crate::program_status::ProgramState::Idle]);
        assert!(queue.len() <= STATUS_EVENT_CAPACITY);
    }
}
