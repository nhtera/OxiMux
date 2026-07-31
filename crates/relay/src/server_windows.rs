//! Windows stand-in for the daemon's accept loop.
//!
//! The relay's transport is a Unix domain socket end to end — bind path, the
//! `SocketGuard` that unlinks it, the 0600 permissions the supervisor relies
//! on. Windows needs named pipes with an explicit owner-only security
//! descriptor, which is its own piece of work rather than a translation.
//!
//! This refuses loudly instead of returning `Ok(())`. A daemon that "starts"
//! and serves nobody would show up as terminals that never open, diagnosed
//! from the wrong end.

use anyhow::{bail, Result};

use crate::server_config::ServerConfig;

pub async fn run_server(_cfg: ServerConfig) -> Result<()> {
    bail!("the relay daemon has no Windows transport yet (named pipes are unimplemented)")
}
