//! The desktop's handle on the shared relay-backed [`TerminalSource`].
//!
//! The implementation lives in `oximux-relay-terminals` (extracted so
//! `oximux serve` exposes the same terminals the same way); what stays here is
//! the desktop's install pattern — a `OnceLock` published from the relay boot,
//! because the relay comes up before the `RemoteControl` global exists and the
//! two are installed in different scopes. This mirrors
//! `install_shared_backend`, which publishes the terminal backend from the
//! same boot step for the same reason.
//!
//! [`TerminalSource`]: oximux_remote_host::TerminalSource

use std::sync::Arc;

pub use oximux_relay_terminals::RelayTerminals;

/// The process-wide terminal source, published by the relay boot.
static INSTALLED: std::sync::OnceLock<Arc<RelayTerminals>> = std::sync::OnceLock::new();

/// Publish the terminal source. Called once, from the relay boot.
pub fn install(terminals: Arc<RelayTerminals>) {
    let _ = INSTALLED.set(terminals);
}

/// The published terminal source, if the relay came up.
///
/// `None` when the daemon failed to start and the app fell back to in-process
/// PTYs. Remote terminals are then simply not served — the fallback backend has
/// no attachment model to expose, and half-serving it would be worse than a
/// clean refusal.
pub fn installed() -> Option<Arc<RelayTerminals>> {
    INSTALLED.get().cloned()
}
