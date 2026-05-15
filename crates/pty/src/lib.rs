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
pub mod events;
pub mod portable_pty_backend;
pub mod snapshot;
pub mod state;

pub use backend::{SpawnConfig, TerminalBackend, TerminalSessionId};
pub use events::TerminalEvent;
pub use portable_pty_backend::PortablePtyBackend;
pub use snapshot::{Cell, TerminalSnapshot};
pub use state::TerminalState;
