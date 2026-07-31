// Relay daemon internals exposed as a lib so integration tests can
// drive the server in-process (spawning a binary per test would slow
// the suite to a crawl). The actual binary entry point is `main.rs`.

pub mod checkpoint;
pub mod codec;
pub mod registry;
pub mod ring_buffer;
pub mod server;
pub mod server_config;

pub use ring_buffer::RingBuffer;
pub use server::run_server;
pub use server_config::{DEFAULT_IDLE_TIMEOUT, ServerConfig};
