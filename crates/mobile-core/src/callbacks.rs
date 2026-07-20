//! The foreign (JS/Swift/Kotlin) callback interfaces the core pushes into.
//!
//! `with_foreign` makes each implementable on the foreign side; the core holds an
//! `Arc<dyn …>` and invokes it from its runtime tasks.

use crate::ffi_types::ConnState;
use crate::snapshot::ThreadSnapshot;

/// Receives the folded transcript for a subscribed session.
///
/// The core pushes the *whole* folded thread rather than individual events, so
/// the app renders the `agent-core` fold the desktop already runs instead of
/// reimplementing one in TypeScript. Pushes are batched: a burst of streaming
/// deltas folds into one snapshot rather than one push per token.
#[uniffi::export(with_foreign)]
pub trait ThreadSink: Send + Sync {
    fn on_thread(&self, snapshot: ThreadSnapshot);
}

/// Receives live frames from attached terminals.
///
/// Raw bytes, not a parsed screen: the app renders them with a real terminal
/// emulator, so the core never reimplements one and never has to keep a grid in
/// sync across the FFI. `pty_id` is carried on every call because one sink
/// serves every attached terminal — the connection has a single push stream.
#[uniffi::export(with_foreign)]
pub trait TerminalSink: Send + Sync {
    /// Bytes drawn by the terminal's process.
    fn on_output(&self, pty_id: String, bytes: Vec<u8>);
    /// The host dropped output destined for this client. The app must re-attach
    /// to resync — anything it renders from here on is a screen with a hole in
    /// it, and nothing downstream can detect that on its own.
    fn on_gap(&self, pty_id: String);
    /// The terminal's process ended.
    fn on_exit(&self, pty_id: String, code: Option<i32>);
}

/// Receives connection-state transitions for the whole client.
#[uniffi::export(with_foreign)]
pub trait ConnStateListener: Send + Sync {
    fn on_state(&self, state: ConnState);
}
