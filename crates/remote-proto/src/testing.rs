//! Test-only in-process transports, gated behind the `testing` feature so other
//! crates (e.g. `remote-host`'s dispatcher tests) can drive the RPC layer with no
//! network. Never compiled into a shippable build.

use async_trait::async_trait;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::lock::Mutex;

use crate::transport::{Transport, TransportError};

/// An in-process loopback endpoint: its send half feeds the paired endpoint's
/// receive half. Build a connected pair with [`duplex_pair`].
pub struct DuplexTransport {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

/// Two endpoints wired to each other — a loopback "connection".
pub fn duplex_pair() -> (DuplexTransport, DuplexTransport) {
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
