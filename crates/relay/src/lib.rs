// Relay daemon internals exposed as a lib so integration tests can
// drive the server in-process (spawning a binary per test would slow
// the suite to a crawl). The actual binary entry point is `main.rs`.

pub mod checkpoint;
pub mod codec;
pub mod registry;
pub mod ring_buffer;
pub mod server_config;

// The accept loop is Unix-domain-socket-shaped down to its drop guards, so
// Windows gets a module that refuses rather than a translation of it. Both
// spell `run_server`, so `main.rs` and the integration tests do not branch.
#[cfg(unix)]
pub mod server;
#[cfg(not(unix))]
#[path = "server_windows.rs"]
pub mod server;

pub use ring_buffer::RingBuffer;
pub use server::run_server;
pub use server_config::{DEFAULT_IDLE_TIMEOUT, ServerConfig};
