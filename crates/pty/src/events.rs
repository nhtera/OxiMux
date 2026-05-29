//! Lifecycle events surfaced by a `TerminalBackend`.
//!
//! Backends accumulate these internally (bounded by the channel capacity)
//! and the UI drains them once per frame via `drain_events`.

use std::path::PathBuf;

use crate::backend::TerminalSessionId;

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
}

impl TerminalEvent {
    /// Session this event is addressed to. Used by backends that
    /// multiplex many panes onto one event source (e.g., the relay
    /// backend) to route a drained event back to the owning view.
    pub fn session_id(&self) -> TerminalSessionId {
        match self {
            TerminalEvent::Output { id, .. }
            | TerminalEvent::Exit { id, .. }
            | TerminalEvent::Resize { id, .. }
            | TerminalEvent::TitleChange { id, .. }
            | TerminalEvent::Bell { id }
            | TerminalEvent::CwdChanged { id, .. }
            | TerminalEvent::CommandMark { id, .. }
            | TerminalEvent::Progress { id, .. }
            | TerminalEvent::Clipboard { id, .. }
            | TerminalEvent::PtyReply { id, .. } => *id,
        }
    }
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
}
