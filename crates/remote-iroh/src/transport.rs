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
///
/// Public because a caller assembling a large request (a prompt carrying images)
/// has to know the ceiling to refuse politely rather than let the send fail as an
/// opaque transport error.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The receive half plus its carry buffer (see the module docs on cancel-safety).
struct RecvHalf {
    stream: RecvStream,
    /// Bytes read off the wire but not yet returned as a complete frame.
    carry: Vec<u8>,
    /// Bytes still owed to an oversize frame being discarded. Non-zero only
    /// between reporting [`TransportError::FrameTooLarge`] and finishing the
    /// resync; every byte read while it is set is dropped, never buffered, so
    /// skipping a huge frame costs no more memory than skipping a small one.
    skip: usize,
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
            recv: Mutex::new(RecvHalf { stream: recv, carry: Vec::new(), skip: 0 }),
            _conn: conn,
        }
    }
}

/// Discard up to `skip` buffered bytes, returning whether the skip finished.
fn advance_skip(carry: &mut Vec<u8>, skip: &mut usize) -> bool {
    let n = (*skip).min(carry.len());
    carry.drain(..n);
    *skip -= n;
    *skip == 0
}

/// Pull one whole frame out of `carry` if a full `[len][body]` is buffered,
/// draining it; `Ok(None)` if more bytes are still needed.
///
/// An over-cap length prefix is **resynchronized rather than fatal**: the prefix
/// says exactly how far the frame runs, so its bytes are marked for discard and
/// the error names the size. Once they have all been dropped the stream is back
/// on a frame boundary and later frames parse normally. Refusing to advance here
/// instead — the obvious reading of "reject the frame" — leaves the stream parked
/// on bytes that can never form a frame, which is a stall dressed up as a check.
fn take_frame(carry: &mut Vec<u8>, skip: &mut usize) -> Result<Option<Vec<u8>>, TransportError> {
    if *skip > 0 && !advance_skip(carry, skip) {
        return Ok(None);
    }
    if carry.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([carry[0], carry[1], carry[2], carry[3]]) as usize;
    if len > MAX_FRAME {
        carry.drain(..4);
        *skip = len;
        advance_skip(carry, skip);
        return Err(TransportError::FrameTooLarge { len, cap: MAX_FRAME });
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
            let RecvHalf { carry, skip, .. } = &mut *half;
            if let Some(frame) = take_frame(carry, skip)? {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wire frame: `[len BE][body]`.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut v = (body.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(body);
        v
    }

    /// `take_frame` against a fresh skip counter, for the cases that never
    /// resynchronize (skip stays 0 throughout).
    fn take(carry: &mut Vec<u8>) -> Result<Option<Vec<u8>>, TransportError> {
        take_frame(carry, &mut 0)
    }

    /// A header for a frame of `len` bytes whose body never follows.
    fn header(len: usize) -> Vec<u8> {
        (len as u32).to_be_bytes().to_vec()
    }

    /// The carry-reassembly core `recv` relies on: fewer than a whole frame's bytes
    /// yields nothing and leaves the carry untouched, so the next read completes it.
    /// This is the deterministic half of the cancel-safety property (the live
    /// dropped-poll path is covered by the over-iroh integration test): partial
    /// bytes staged in `carry` are never lost between reads.
    #[test]
    fn a_partial_frame_is_buffered_until_complete() {
        let full = framed(b"hello"); // [0,0,0,5][h,e,l,l,o] = 9 bytes
        let mut carry: Vec<u8> = full[..6].to_vec(); // len prefix + 2 body bytes

        // Header known but body short → nothing yet, and the bytes are kept.
        assert_eq!(take(&mut carry).unwrap(), None);
        assert_eq!(carry.len(), 6, "the partial frame stays staged");

        // The rest arrives → the whole frame comes out, carry drained.
        carry.extend_from_slice(&full[6..]);
        assert_eq!(take(&mut carry).unwrap(), Some(b"hello".to_vec()));
        assert!(carry.is_empty());
    }

    /// A read that delivers more than one frame yields them one at a time, leaving
    /// the trailing frame's bytes in the carry for the next call.
    #[test]
    fn extra_bytes_after_a_frame_stay_for_the_next() {
        let mut carry = framed(b"ab");
        carry.extend_from_slice(&framed(b"cde"));

        assert_eq!(take(&mut carry).unwrap(), Some(b"ab".to_vec()));
        assert_eq!(take(&mut carry).unwrap(), Some(b"cde".to_vec()));
        assert_eq!(take(&mut carry).unwrap(), None);
    }

    /// Fewer than the 4 header bytes can't even name a length → buffer and wait.
    #[test]
    fn a_truncated_header_waits_for_more() {
        let mut carry = vec![0u8, 0, 0]; // 3 of the 4 length bytes
        assert_eq!(take(&mut carry).unwrap(), None);
        assert_eq!(carry.len(), 3);
    }

    /// A hostile/corrupt length prefix above the cap is rejected rather than driving
    /// an unbounded allocation, and names both sizes so the report is actionable.
    #[test]
    fn an_oversize_length_prefix_is_rejected() {
        let mut carry = header(MAX_FRAME + 1);
        let mut skip = 0;
        assert!(matches!(
            take_frame(&mut carry, &mut skip),
            Err(TransportError::FrameTooLarge { len, cap })
                if len == MAX_FRAME + 1 && cap == MAX_FRAME
        ));
    }

    /// The bug this guards: one over-cap reply used to end the connection, taking
    /// every unrelated subscription on the multiplexed transport with it. The
    /// oversize frame's bytes are discarded instead, and the frame behind it —
    /// which may be a live event for a different session — still parses.
    #[test]
    fn the_stream_resynchronizes_after_an_oversize_frame() {
        let body = [0xABu8; 32];
        let mut carry = header(MAX_FRAME + 1);
        carry.extend_from_slice(&body[..10]); // first bytes of the doomed frame
        let mut skip = 0;

        assert!(take_frame(&mut carry, &mut skip).is_err(), "reported once");
        assert!(carry.is_empty(), "the buffered part of it was dropped, not retained");
        assert_eq!(skip, MAX_FRAME + 1 - 10, "the rest is still owed");

        // The remainder of the oversize frame trickles in and is swallowed whole.
        let mut remaining = skip;
        while remaining > 0 {
            let n = remaining.min(8192);
            carry.extend(std::iter::repeat_n(0xABu8, n));
            remaining -= n;
            assert_eq!(take_frame(&mut carry, &mut skip).unwrap(), None, "still skipping");
        }
        assert_eq!(skip, 0, "resynchronized");

        // Back on a frame boundary: the next frame parses normally.
        carry.extend_from_slice(&framed(b"next"));
        assert_eq!(take_frame(&mut carry, &mut skip).unwrap(), Some(b"next".to_vec()));
    }

    /// An oversize frame that arrives fully buffered alongside the next one must
    /// not eat into its neighbour — the skip is exactly the declared length.
    #[test]
    fn resynchronizing_consumes_only_the_oversize_frame() {
        // A cap-sized skip is impractical to buffer in a test, so drive the same
        // arithmetic through `advance_skip`, which is what bounds the discard.
        let mut carry = vec![0u8; 10];
        carry.extend_from_slice(&framed(b"kept"));
        let mut skip = 10;

        assert!(advance_skip(&mut carry, &mut skip), "the skip completes");
        assert_eq!(take_frame(&mut carry, &mut skip).unwrap(), Some(b"kept".to_vec()));
    }
}
