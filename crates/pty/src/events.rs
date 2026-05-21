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

impl TerminalEvent {
    /// Session this event is addressed to. Used by backends that
    /// multiplex many panes onto one event source (e.g., the relay
    /// backend) to route a drained event back to the owning view.
    pub fn session_id(&self) -> TerminalSessionId {
        match self {
            TerminalEvent::Output { id, .. }
            | TerminalEvent::Exit { id, .. }
            | TerminalEvent::Resize { id, .. }
            | TerminalEvent::TitleChange { id, .. } => *id,
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
