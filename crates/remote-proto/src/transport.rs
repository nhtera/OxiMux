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
    ///
    /// **Must be cancel-safe:** a caller may drop the returned future before it
    /// resolves (e.g. a read loop that `select`s `recv` against a shutdown signal),
    /// and doing so must not consume or lose a frame — the next `recv` must still
    /// yield it. The in-memory loopback satisfies this (its receive is a single
    /// channel poll); a stream-based impl (iroh) must buffer any partially-read
    /// frame rather than discard bytes on drop.
    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Open a one-way stream to the peer, for bulk data that must not queue
    /// behind (or ahead of) the framed messages: a device's picture. Each
    /// stream is independent of this channel and of every other stream, so a
    /// stalled one holds up nothing else.
    ///
    /// A transport without streams (a local socket) answers
    /// [`TransportError::Unsupported`], the default.
    async fn open_uni(&self) -> Result<Box<dyn UniSend>, TransportError> {
        Err(TransportError::Unsupported)
    }

    /// Wait for the next one-way stream the peer opens. `Ok(None)` once the
    /// connection is gone. Same default as [`open_uni`](Self::open_uni).
    async fn accept_uni(&self) -> Result<Option<Box<dyn UniRecv>>, TransportError> {
        Err(TransportError::Unsupported)
    }

    /// Whether the connection runs through a relay server rather than
    /// directly between the two ends (slower, and worth telling the user
    /// when it carries video). `None` where the question has no meaning.
    fn relayed(&self) -> Option<bool> {
        None
    }
}

/// The writing end of a one-way stream ([`Transport::open_uni`]). Dropping it
/// ends the stream.
#[async_trait]
pub trait UniSend: Send {
    /// Write all of `bytes`, waiting while the peer is not keeping up. Fails
    /// once the peer stopped reading or the connection is gone.
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError>;
}

/// The reading end of a one-way stream ([`Transport::accept_uni`]). Dropping
/// it tells the peer to stop sending.
#[async_trait]
pub trait UniRecv: Send {
    /// Read some bytes into `buf`: how many, or `None` once the stream ended.
    async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TransportError>;
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
    /// The peer announced a frame larger than this transport will assemble.
    ///
    /// **Recoverable, unlike the variants above.** The frame's length prefix says
    /// exactly how many bytes to discard, so the transport resynchronizes by
    /// dropping them and the stream stays usable — a caller must report this
    /// against the one call it belongs to and keep reading. Treating it as fatal
    /// costs the whole connection over a single reply, which for a multiplexed
    /// transport also takes down every unrelated subscription riding it.
    #[error("the peer sent a {len}-byte frame, over the {cap}-byte limit")]
    FrameTooLarge { len: usize, cap: usize },
    /// This transport has no one-way streams ([`Transport::open_uni`]).
    #[error("this connection cannot carry a separate stream")]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Request, Response};
    use crate::testing::duplex_pair;
    use futures::{executor::block_on, future::join};

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
