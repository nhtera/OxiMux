//! What the daemon is configured with, separate from how it serves.
//!
//! Split out of `server` because the serving machinery is Unix-socket-shaped
//! and does not compile on Windows yet, while `main.rs` builds a config on
//! every platform. Keeping the two together would have forced the argument
//! parser behind the same platform gate as the accept loop.

use std::path::PathBuf;
use std::time::Duration;

// Daemon self-exit if no clients attached and no PTYs alive for this
// long. Plan's "Locked decisions": 24h. Configurable for tests via
// `ServerConfig.idle_timeout`.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 24);

pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub token_file: PathBuf,
    // None skips PID-file writing (useful for in-process tests).
    pub pid_path: Option<PathBuf>,
    // None disables idle GC. Tests override with short values to
    // exercise the auto-exit path without sleeping for a day.
    pub idle_timeout: Option<Duration>,
    // None uses `DEFAULT_IDLE_TICK`. Tests can pick a sub-second tick
    // so the idle timer reaches its threshold quickly.
    pub idle_tick_interval: Option<Duration>,
    // Root directory for per-PTY disk scrollback checkpoints. None
    // disables checkpointing entirely (in-process tests).
    pub checkpoint_dir: Option<PathBuf>,
    // None uses `CHECKPOINT_TICK`. Tests pick sub-second ticks so a
    // checkpoint lands without multi-second sleeps.
    pub checkpoint_tick_interval: Option<Duration>,
}

impl ServerConfig {
    pub fn idle_disabled(socket_path: PathBuf, token_file: PathBuf) -> Self {
        Self {
            socket_path,
            token_file,
            pid_path: None,
            idle_timeout: None,
            idle_tick_interval: None,
            checkpoint_dir: None,
            checkpoint_tick_interval: None,
        }
    }
}
