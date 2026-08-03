//! The framed [`Transport`] over one local-socket stream.
//!
//! Mirrors `remote-iroh`'s transport byte-for-byte at the framing layer —
//! u32-BE length prefix, the same 16 MiB cap, and the same carry-buffer
//! cancel-safety story — so the dispatcher sees identical behavior whichever
//! transport carried the connection. Named pipes have no half-shutdown, so
//! nothing here ever treats stream EOF as a frame boundary: a frame is
//! delimited by its prefix alone (the relay's discipline).

use async_trait::async_trait;
use futures::lock::Mutex;
use interprocess::local_socket::tokio::{RecvHalf as StreamRecv, SendHalf as StreamSend};
use oximux_remote_proto::transport::{Transport, TransportError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One frame's hard ceiling — the same value as `remote-iroh`'s, so a payload
/// that fits over the network never fails over the faster local socket.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The receive half plus its carry buffer.
struct RecvHalf {
    stream: StreamRecv,
    /// Bytes read off the socket but not yet returned as a complete frame.
    /// Reads land here *before* parsing, so a caller dropping `recv` mid-await
    /// loses nothing — the next call resumes from the buffered bytes.
    carry: Vec<u8>,
    /// Bytes still owed to an oversize frame being discarded (resync after
    /// [`TransportError::FrameTooLarge`]); dropped, never buffered.
    skip: usize,
}

/// A framed [`Transport`] over one accepted/dialed local-socket stream.
/// Shareable as `Arc<dyn Transport>`; send and receive serialize independently.
pub struct LocalSocketTransport {
    send: Mutex<StreamSend>,
    recv: Mutex<RecvHalf>,
}

impl LocalSocketTransport {
    /// Wrap a connected stream's halves (listener accept / client connect).
    pub(crate) fn new(send: StreamSend, recv: StreamRecv) -> Self {
        Self {
            send: Mutex::new(send),
            recv: Mutex::new(RecvHalf { stream: recv, carry: Vec::new(), skip: 0 }),
        }
    }
}

// Opaque on purpose: the halves have nothing printable, but callers hold this
// inside `Result`s whose `unwrap_err` wants `Debug`.
impl std::fmt::Debug for LocalSocketTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalSocketTransport")
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
/// draining it; `Ok(None)` if more bytes are still needed. An over-cap prefix
/// resynchronizes (marks its body for discard) rather than killing the stream
/// — see `remote-iroh`, whose logic this is.
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
impl Transport for LocalSocketTransport {
    async fn send(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        if frame.len() > MAX_FRAME {
            return Err(TransportError::FrameTooLarge { len: frame.len(), cap: MAX_FRAME });
        }
        let len = u32::try_from(frame.len())
            .map_err(|_| TransportError::Io("frame larger than u32".into()))?;
        let mut send = self.send.lock().await;
        send.write_all(&len.to_be_bytes()).await.map_err(write_err)?;
        send.write_all(&frame).await.map_err(write_err)?;
        send.flush().await.map_err(write_err)?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut half = self.recv.lock().await;
        loop {
            // Fast path: a whole frame is already buffered — no await, so a
            // drop on this path cannot lose anything.
            let RecvHalf { carry, skip, .. } = &mut *half;
            if let Some(frame) = take_frame(carry, skip)? {
                return Ok(Some(frame));
            }
            // Read more. The bytes land in `buf` and are appended to `carry`
            // in the same poll that completes the read, so cancellation can
            // only strike where nothing has been consumed yet.
            let mut buf = [0u8; 16 * 1024];
            let n = half.stream.read(&mut buf).await.map_err(read_err)?;
            if n == 0 {
                // Clean close only between frames; mid-frame it means the peer
                // died — surface that rather than pretending it hung up tidily.
                return if half.carry.is_empty() && half.skip == 0 {
                    Ok(None)
                } else {
                    Err(TransportError::Io("peer closed mid-frame".into()))
                };
            }
            half.carry.extend_from_slice(&buf[..n]);
        }
    }
}

fn write_err(e: std::io::Error) -> TransportError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        TransportError::Closed
    } else {
        TransportError::Io(e.to_string())
    }
}

fn read_err(e: std::io::Error) -> TransportError {
    TransportError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure frame parser: whole frames come out, partials wait, an
    /// oversize prefix resyncs without ending the stream.
    #[test]
    fn take_frame_parses_resyncs_and_waits() {
        let mut carry = Vec::new();
        let mut skip = 0usize;

        // Partial prefix: wait.
        carry.extend_from_slice(&[0, 0]);
        assert!(matches!(take_frame(&mut carry, &mut skip), Ok(None)));

        // Whole small frame.
        carry.clear();
        carry.extend_from_slice(&3u32.to_be_bytes());
        carry.extend_from_slice(b"abc");
        assert_eq!(take_frame(&mut carry, &mut skip).unwrap(), Some(b"abc".to_vec()));

        // Oversize prefix: error names the size, then the stream resyncs onto
        // the next frame once the body has been discarded.
        let huge = (MAX_FRAME + 1) as u32;
        carry.extend_from_slice(&huge.to_be_bytes());
        assert!(matches!(
            take_frame(&mut carry, &mut skip),
            Err(TransportError::FrameTooLarge { len, cap }) if len == MAX_FRAME + 1 && cap == MAX_FRAME
        ));
        // Feed the discarded body in two chunks, then a good frame.
        carry.extend_from_slice(&vec![0u8; MAX_FRAME / 2]);
        assert!(matches!(take_frame(&mut carry, &mut skip), Ok(None)));
        carry.extend_from_slice(&vec![0u8; MAX_FRAME / 2 + 1]);
        carry.extend_from_slice(&2u32.to_be_bytes());
        carry.extend_from_slice(b"ok");
        assert_eq!(take_frame(&mut carry, &mut skip).unwrap(), Some(b"ok".to_vec()));
    }
}
