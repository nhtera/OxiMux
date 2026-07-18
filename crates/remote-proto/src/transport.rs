//! The transport-agnostic seam: a framed, bidirectional byte channel the RPC
//! dispatcher and client speak over, independent of what carries the bytes.
//!
//! iroh (a QUIC bi-stream) is the production impl, but the dispatcher depends
//! only on this trait — so an in-memory loopback can drive integration tests
//! with no network, and a WebSocket transport stays a future drop-in. The
//! interface is **frame-oriented** (one `Vec<u8>` per postcard message), not a
//! raw byte stream: the RPC layer already deals in discrete
//! [`Request`](crate::proto::Request)/[`Response`](crate::proto::Response)
//! messages, so framing belongs to the transport, not every handler.

use async_trait::async_trait;

/// A framed, bidirectional message channel to one peer.
///
/// `&self` on both methods (not `&mut`) so a connection can be shared as
/// `Arc<dyn Transport>` between a reader and a writer task; impls provide their
/// own interior mutability for the receive half.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one framed message to the peer.
    async fn send(&self, frame: Vec<u8>) -> Result<(), TransportError>;

    /// Receive the next framed message, or `None` once the peer has closed the
    /// channel and no more frames will arrive.
    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError>;
}

/// A transport-layer failure, distinct from the wire-codec
/// [`WireError`](crate::proto::WireError) and the protocol-level
/// [`RpcError`](crate::proto::RpcError).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The channel is closed — the peer went away or the stream was reset.
    #[error("transport closed")]
    Closed,
    /// An underlying I/O failure (stringified: impls span different error types).
    #[error("transport io error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Request, Response};
    use futures::channel::mpsc;
    use futures::lock::Mutex;
    use futures::{StreamExt, executor::block_on, future::join};

    /// An in-process loopback: two endpoints whose send halves feed the other's
    /// receive half. Enough to drive the dispatcher end-to-end without a network.
    struct DuplexTransport {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    }

    fn duplex_pair() -> (DuplexTransport, DuplexTransport) {
        let (a_tx, b_rx) = mpsc::unbounded();
        let (b_tx, a_rx) = mpsc::unbounded();
        (
            DuplexTransport { tx: a_tx, rx: Mutex::new(a_rx) },
            DuplexTransport { tx: b_tx, rx: Mutex::new(b_rx) },
        )
    }

    #[async_trait]
    impl Transport for DuplexTransport {
        async fn send(&self, frame: Vec<u8>) -> Result<(), TransportError> {
            // `unbounded_send` fails only if every receiver was dropped.
            self.tx.unbounded_send(frame).map_err(|_| TransportError::Closed)
        }
        async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(self.rx.lock().await.next().await)
        }
    }

    /// The seam proof: a real `Request` postcard-frame crosses an abstract
    /// `Transport` and comes back as a decoded `Response`, with the dispatch side
    /// blind to the concrete transport.
    #[test]
    fn request_response_round_trips_over_a_transport() {
        block_on(async {
            let (client, server) = duplex_pair();

            let serve = async {
                let frame = server.recv().await.unwrap().expect("a request frame");
                let req = Request::from_bytes(&frame).unwrap();
                assert_eq!(req, Request::Ping);
                server.send(Response::Pong.to_bytes().unwrap()).await.unwrap();
            };
            let call = async {
                client.send(Request::Ping.to_bytes().unwrap()).await.unwrap();
                let frame = client.recv().await.unwrap().expect("a response frame");
                assert_eq!(Response::from_bytes(&frame).unwrap(), Response::Pong);
            };
            join(serve, call).await;
        });
    }

    /// A closed peer is observable, not a hang: `recv` yields `None` and a
    /// subsequent `send` reports `Closed` rather than silently dropping.
    #[test]
    fn closed_peer_is_reported() {
        block_on(async {
            let (client, server) = duplex_pair();
            drop(client);
            assert!(server.recv().await.unwrap().is_none(), "recv ends at None on close");
            assert!(
                matches!(server.send(vec![1, 2, 3]).await, Err(TransportError::Closed)),
                "send after peer close is a Closed error"
            );
        });
    }
}
