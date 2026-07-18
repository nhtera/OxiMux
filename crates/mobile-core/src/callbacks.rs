//! The foreign (JS/Swift/Kotlin) callback interfaces the core pushes into.
//!
//! `with_foreign` makes each implementable on the foreign side; the core holds an
//! `Arc<dyn …>` and invokes it from its runtime tasks.

use crate::ffi_types::{ConnState, RemoteEvent};

/// Receives folded live events for a subscribed session, in seq order.
#[uniffi::export(with_foreign)]
pub trait EventSink: Send + Sync {
    fn on_event(&self, event: RemoteEvent);
}

/// Receives connection-state transitions for the whole client.
#[uniffi::export(with_foreign)]
pub trait ConnStateListener: Send + Sync {
    fn on_state(&self, state: ConnState);
}
