// Relay daemon internals exposed as a lib so integration tests can
// drive the server in-process (spawning a binary per test would slow
// the suite to a crawl). The actual binary entry point is `main.rs`.

pub mod checkpoint;
pub mod codec;
// Whether this process can still reach the OS lookup daemons — and therefore
// whether the PTYs it spawns can. Unix-only: the failure it watches for is an
// inherited Mach bootstrap port going dead, which has no Windows equivalent.
#[cfg(unix)]
pub mod host_lookup;
pub mod registry;
pub mod ring_buffer;
// The pipe's access control. Windows-only because it is the platform that
// needs one stated: a unix socket is protected by the directory holding it.
#[cfg(windows)]
pub mod pipe_security;
pub mod server;
pub mod server_config;

pub use ring_buffer::RingBuffer;
pub use server::run_server;
pub use server_config::{DEFAULT_IDLE_TIMEOUT, ServerConfig};
