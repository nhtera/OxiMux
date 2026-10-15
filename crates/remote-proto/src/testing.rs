//! Test-only in-process transports, gated behind the `testing` feature so other
//! crates (e.g. `remote-host`'s dispatcher tests) can drive the RPC layer with no
//! network. Never compiled into a shippable build.

use async_trait::async_trait;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::lock::Mutex;

use crate::transport::{Transport, TransportError, UniRecv, UniSend};

/// An in-process loopback endpoint: its send half feeds the paired endpoint's
/// receive half. Build a connected pair with [`duplex_pair`].
pub struct DuplexTransport {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// One-way streams this end opens arrive at the peer's `uni_rx`.
    uni_tx: mpsc::UnboundedSender<mpsc::UnboundedReceiver<Vec<u8>>>,
    uni_rx: Mutex<mpsc::UnboundedReceiver<mpsc::UnboundedReceiver<Vec<u8>>>>,
}

/// Two endpoints wired to each other — a loopback "connection".
pub fn duplex_pair() -> (DuplexTransport, DuplexTransport) {
    let (a_tx, b_rx) = mpsc::unbounded();
    let (b_tx, a_rx) = mpsc::unbounded();
    let (a_uni_tx, b_uni_rx) = mpsc::unbounded();
    let (b_uni_tx, a_uni_rx) = mpsc::unbounded();
    (
        DuplexTransport { tx: a_tx, rx: Mutex::new(a_rx), uni_tx: a_uni_tx, uni_rx: Mutex::new(a_uni_rx) },
        DuplexTransport { tx: b_tx, rx: Mutex::new(b_rx), uni_tx: b_uni_tx, uni_rx: Mutex::new(b_uni_rx) },
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
    async fn open_uni(&self) -> Result<Box<dyn UniSend>, TransportError> {
        let (tx, rx) = mpsc::unbounded();
        self.uni_tx.unbounded_send(rx).map_err(|_| TransportError::Closed)?;
        Ok(Box::new(DuplexUniSend(tx)))
    }
    async fn accept_uni(&self) -> Result<Option<Box<dyn UniRecv>>, TransportError> {
        let next = self.uni_rx.lock().await.next().await;
        Ok(next.map(|rx| Box::new(DuplexUniRecv { rx, carry: Vec::new() }) as Box<dyn UniRecv>))
    }
}

/// A loopback one-way stream's writing end. Writing fails once the reader is
/// dropped, as a QUIC stream does once its peer stops reading.
struct DuplexUniSend(mpsc::UnboundedSender<Vec<u8>>);

#[async_trait]
impl UniSend for DuplexUniSend {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.0.unbounded_send(bytes.to_vec()).map_err(|_| TransportError::Closed)
    }
}

/// A loopback one-way stream's reading end.
struct DuplexUniRecv {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// The rest of a chunk larger than the last read's buffer.
    carry: Vec<u8>,
}

#[async_trait]
impl UniRecv for DuplexUniRecv {
    async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TransportError> {
        if self.carry.is_empty() {
            match self.rx.next().await {
                Some(chunk) => self.carry = chunk,
                None => return Ok(None),
            }
        }
        let n = buf.len().min(self.carry.len());
        buf[..n].copy_from_slice(&self.carry[..n]);
        self.carry.drain(..n);
        Ok(Some(n))
    }
}
