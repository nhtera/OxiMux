//! The dial seam: how the reconnect driver obtains a fresh transport to the host.
//!
//! Each (re)connect attempt asks the [`Connector`] for a new [`Transport`]. The
//! production impl is an iroh endpoint dial — a fresh QUIC bi-stream with a fresh
//! pkarr resolve every attempt, so a moved/renumbered host is re-found on retry;
//! tests use the in-memory loopback. Keeping the dial behind a trait is what lets
//! the reconnect policy ([`crate::reconnect`]) and its driver stay transport- and
//! network-agnostic.

use std::sync::Arc;

use async_trait::async_trait;
use oximux_remote_proto::Transport;

/// Produces a fresh [`Transport`] to the host on demand — one per (re)connect
/// attempt.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Dial the host and return a live framed transport, or fail if it could not
    /// be reached this attempt (the reconnect policy then backs off and retries).
    async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError>;
}

/// A dial failure — the host could not be reached on this attempt. Distinct from a
/// [`SessionError`](crate::SessionError): that is a fault on an *established*
/// connection, this is a failure to establish one.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// No route to the host — resolve failed, the dial timed out, or the peer
    /// refused. Carries a human-readable cause for the UI's unreachable state.
    #[error("host unreachable: {0}")]
    Unreachable(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use oximux_remote_proto::TransportError;

    /// A trivial transport so the mock connector can hand back an object.
    struct StubTransport;

    #[async_trait]
    impl Transport for StubTransport {
        async fn send(&self, _frame: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
    }

    /// A connector scripted to fail a fixed number of times, then succeed — the
    /// shape the reconnect driver exercises.
    struct FlakyConnector {
        fail_first: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl Connector for FlakyConnector {
        async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError> {
            use std::sync::atomic::Ordering;
            // Decrement the fail budget while it lasts; then succeed.
            if self
                .fail_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                Err(ConnectError::Unreachable("no route".into()))
            } else {
                Ok(Arc::new(StubTransport))
            }
        }
    }

    #[test]
    fn connector_is_object_safe_and_reports_both_outcomes() {
        let connector: Box<dyn Connector> =
            Box::new(FlakyConnector { fail_first: 2.into() });
        assert!(matches!(block_on(connector.connect()), Err(ConnectError::Unreachable(_))));
        assert!(matches!(block_on(connector.connect()), Err(ConnectError::Unreachable(_))));
        assert!(block_on(connector.connect()).is_ok(), "succeeds once the fail budget is spent");
    }
}
