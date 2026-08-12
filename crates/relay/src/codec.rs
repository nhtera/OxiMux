// Async wrapper around `oximux_relay_proto::{encode_frame, decode_frame}`.
// Lives in the daemon (and is copied client-side in phase 03) instead
// of in the proto crate so the proto crate stays runtime-agnostic.

use oximux_relay_proto::{Frame, ProtoError, decode_frame, encode_frame};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
    #[error("peer closed the connection cleanly")]
    Eof,
    #[error("peer closed mid-frame ({0} bytes buffered)")]
    UnexpectedEof(usize),
}

// Initial read-buffer capacity. Most frames are small (a few hundred
// bytes); we grow on demand for larger Output / AttachOk payloads.
const READ_CHUNK: usize = 8 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Frame, CodecError> {
    loop {
        if let Some((frame, consumed)) = decode_frame(buf)? {
            buf.drain(..consumed);
            return Ok(frame);
        }
        let mut chunk = [0u8; READ_CHUNK];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            // EOF on a frame boundary is a normal client disconnect.
            // EOF with bytes still in the buffer means the peer cut
            // the connection mid-frame — a protocol error worth
            // surfacing distinctly.
            if buf.is_empty() {
                return Err(CodecError::Eof);
            }
            return Err(CodecError::UnexpectedEof(buf.len()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), CodecError> {
    let bytes = encode_frame(frame)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_relay_proto::{Notification, Request};
    use tokio::io::duplex;

    #[tokio::test]
    async fn round_trip_one_frame_over_duplex() {
        let (mut a, mut b) = duplex(1024);
        let frame = Frame::Request {
            request_id: 42,
            request: Request::ListPtys,
        };
        write_frame(&mut a, &frame).await.unwrap();
        let mut buf = Vec::new();
        let got = read_frame(&mut b, &mut buf).await.unwrap();
        assert_eq!(got, frame);
    }

    #[tokio::test]
    async fn round_trip_two_frames_back_to_back() {
        let (mut a, mut b) = duplex(4096);
        let f1 = Frame::Notification(Notification::Exit {
            attachment_id: 0,
            pty_id: "x".into(),
            code: Some(0),
        });
        let f2 = Frame::Notification(Notification::Output {
            attachment_id: 0,
            pty_id: "x".into(),
            bytes: vec![1, 2, 3, 4],
        });
        write_frame(&mut a, &f1).await.unwrap();
        write_frame(&mut a, &f2).await.unwrap();
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), f1);
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), f2);
    }

    #[tokio::test]
    async fn eof_returns_error() {
        let (a, mut b) = duplex(64);
        drop(a);
        let mut buf = Vec::new();
        assert!(matches!(
            read_frame(&mut b, &mut buf).await,
            Err(CodecError::Eof)
        ));
    }
}
