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

/// Receives connection-state transitions for the whole client.
#[uniffi::export(with_foreign)]
pub trait ConnStateListener: Send + Sync {
    fn on_state(&self, state: ConnState);
}
