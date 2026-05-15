//! Lifecycle events surfaced by a `TerminalBackend`.
//!
//! Backends accumulate these internally (bounded by the channel capacity)
//! and the UI drains them once per frame via `drain_events`.

use crate::backend::TerminalSessionId;

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
}
