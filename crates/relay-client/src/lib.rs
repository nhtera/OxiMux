// App-side counterpart to `crates/relay`. Two layers:
//
// - `RelayClient` — async multiplexer over the unix socket. Owns the
//   socket, demuxes responses by request_id, fans Notifications to
//   per-PTY subscribers.
// - `RelayBackend` — sync `TerminalBackend` impl. Wraps the client so
//   the existing `TerminalView` works unchanged. Maintains a mirror
//   `TerminalState` per session for snapshot/search/serialize calls
//   that should stay local (no socket round trip).

pub mod backend;
pub mod client;
mod codec;

pub use backend::RelayBackend;
pub use client::{ClientError, RelayClient};
