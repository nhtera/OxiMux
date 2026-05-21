// Async framing wrapper around `oximux_relay_proto::{encode_frame,
// decode_frame}`. Mirrors `crates/relay/src/codec.rs` (the daemon
// side) — kept in two places intentionally so the client doesn't
// have to depend on the daemon's lib crate.

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
