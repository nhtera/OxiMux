//! A [`Transport`] over a single iroh QUIC bi-directional stream.
//!
//! iroh hands us a raw ordered byte pipe; the `Transport` contract is
//! frame-oriented (one postcard message per `send`/`recv`), so this impl owns the
//! framing: a `u32` big-endian length prefix followed by the message bytes. One
//! long-lived bi-stream carries every frame — not a stream per message — so the
//! demux's reply-in-send-order invariant (the wire has no correlation id) holds.
//!
//! The receive half keeps a `carry` buffer of bytes pulled off the wire but not
//! yet returned as a whole frame. That buffer is what makes [`IrohTransport::recv`]
//! **cancel-safe** (the demux pump drops a half-polled `recv` on its shutdown
//! `select`): read bytes always land in the struct-owned `carry` before any yield,
//! so dropping the future can never lose a partially-read frame.

use async_trait::async_trait;
use futures::lock::Mutex;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use oximux_remote_proto::transport::{Transport, TransportError};

/// A hard cap on a single frame so a hostile or corrupt length prefix can't drive
/// an unbounded allocation. Comfortably above any real RPC/event payload.
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The receive half plus its carry buffer (see the module docs on cancel-safety).
struct RecvHalf {
    stream: RecvStream,
    /// Bytes read off the wire but not yet returned as a complete frame.
    carry: Vec<u8>,
}

/// A framed [`Transport`] over one iroh bi-stream. Cloneable-shareable as
/// `Arc<dyn Transport>`; the send and receive halves have independent interior
/// mutability so the writer (RPC callers) and reader (the demux pump) never block
/// each other, only serialize their own half.
pub struct IrohTransport {
    send: Mutex<SendStream>,
    recv: Mutex<RecvHalf>,
    /// Held for the stream's lifetime — dropping the `Connection` resets the
    /// streams, so keep it alive alongside them.
    _conn: Connection,
}

impl IrohTransport {
    /// Wrap an already-opened bi-stream (client `open_bi` / host `accept_bi`).
    pub fn new(conn: Connection, send: SendStream, recv: RecvStream) -> Self {
        Self {
            send: Mutex::new(send),
            recv: Mutex::new(RecvHalf { stream: recv, carry: Vec::new() }),
            _conn: conn,
        }
    }
}

/// Pull one whole frame out of `carry` if a full `[len][body]` is buffered,
/// draining it; `Ok(None)` if more bytes are still needed.
fn take_frame(carry: &mut Vec<u8>) -> Result<Option<Vec<u8>>, TransportError> {
    if carry.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([carry[0], carry[1], carry[2], carry[3]]) as usize;
    if len > MAX_FRAME {
        return Err(TransportError::Io(format!("frame length {len} exceeds cap {MAX_FRAME}")));
    }
    if carry.len() < 4 + len {
        return Ok(None);
    }
    let frame = carry[4..4 + len].to_vec();
    carry.drain(..4 + len);
    Ok(Some(frame))
}

#[async_trait]
impl Transport for IrohTransport {
    async fn send(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        let len = u32::try_from(frame.len())
            .map_err(|_| TransportError::Io("frame larger than u32".into()))?;
        let mut send = self.send.lock().await;
        send.write_all(&len.to_be_bytes()).await.map_err(write_err)?;
        send.write_all(&frame).await.map_err(write_err)?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut half = self.recv.lock().await;
        loop {
            // Fast path: a whole frame is already buffered — no await, so a drop
            // on this path cannot lose anything.
            if let Some(frame) = take_frame(&mut half.carry)? {
                return Ok(Some(frame));
            }
            // Need more bytes. The stream read is the ONLY await; bytes land in the
            // struct-owned `carry` synchronously after it returns (no await between
            // the read and the extend), so cancelling here preserves progress.
            let mut buf = [0u8; 8192];
            match half.stream.read(&mut buf).await.map_err(read_err)? {
                Some(n) => half.carry.extend_from_slice(&buf[..n]),
                // Stream finished: clean close only if no partial frame is dangling.
                None => {
                    return if half.carry.is_empty() {
                        Ok(None)
                    } else {
                        Err(TransportError::Closed)
                    };
                }
            }
        }
    }
}

/// A write failure on a closed/reset stream is a clean `Closed` (the demux reads it
/// as "reconnect"); anything else is a real I/O fault we must not mask.
fn write_err(e: iroh::endpoint::WriteError) -> TransportError {
    use iroh::endpoint::WriteError;
    match e {
        WriteError::ConnectionLost(_) | WriteError::ClosedStream | WriteError::Stopped(_) => {
            TransportError::Closed
        }
        other => TransportError::Io(other.to_string()),
    }
}

/// A read failure on a reset/closed stream is `Closed`; anything else is I/O.
fn read_err(e: iroh::endpoint::ReadError) -> TransportError {
    use iroh::endpoint::ReadError;
    match e {
        ReadError::ConnectionLost(_) | ReadError::ClosedStream | ReadError::Reset(_) => {
            TransportError::Closed
        }
        other => TransportError::Io(other.to_string()),
    }
}
