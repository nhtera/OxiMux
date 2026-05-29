//! oximux-pty
//!
//! Terminal backend abstraction + a `portable-pty` concrete implementation.
//! The `TerminalBackend` trait is the seam that lets the UI render against
//! a generic source: real PTY today, replay fixtures in tests, ACP-driven
//! streams later.
//!
//! Phase 1 step 1-2 status: trait + types + portable-pty backend with
//! tokio reader and bounded event queue. `alacritty_terminal` integration
//! (the actual grid/scrollback state machine) lands in step 3.

pub mod backend;
pub(crate) mod close_grace;
pub mod events;
pub mod grid_serializer;
pub mod osc7;
pub mod portable_pty_backend;
pub mod snapshot;
pub mod state;

pub use backend::{InputMode, MouseMode, SpawnConfig, TerminalBackend, TerminalSessionId};
pub use events::{CommandMarkKind, TerminalEvent};
pub use grid_serializer::{
    CAPTURE_HEADER_LEN, CAPTURE_HEADER_MAGIC, SerializeOptions, build_capture_header,
    parse_capture_header, serialize_term, serialize_term_capped, serialize_term_capped_with_dims,
};
pub use portable_pty_backend::PortablePtyBackend;
pub use snapshot::{Cell, CellColor, CursorShapeKind, NamedColor16, TerminalSnapshot};
pub use state::TerminalState;
