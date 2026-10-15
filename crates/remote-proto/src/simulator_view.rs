//! Watching a device from the paired phone (v34): the RPC payloads, and the
//! framing of the one-way stream that carries the picture.
//!
//! The RPCs ride the connection's one bi-stream like every other call
//! ([`Request::SimViewables`], [`Request::SimSubscribe`],
//! [`Request::SimUnsubscribe`]). The picture never does: each subscription
//! gets a QUIC uni-stream of its own, opened by the host, so a key frame in
//! flight never holds up a terminal's keystrokes behind it.
//!
//! A view stream is the subscription's 16-byte token, then frames:
//!
//! ```text
//! [u32 len, big-endian] [u8 kind] [u64 pts_us, big-endian] [payload: len - 9 bytes]
//! ```
//!
//! `kind` is [`SimViewKind`]. A `Config` payload is an avcC record (what
//! WebCodecs takes as `description`), and every subscription's stream starts
//! with one. `Key` and `Delta` payloads are AVCC samples (4-byte lengths).
//! Pictures arrive as the device shows them, already turned: a device on
//! its side streams landscape frames, so the phone draws what it decodes.
//!
//! [`Request::SimViewables`]: crate::proto::Request::SimViewables
//! [`Request::SimSubscribe`]: crate::proto::Request::SimSubscribe
//! [`Request::SimUnsubscribe`]: crate::proto::Request::SimUnsubscribe

use serde::{Deserialize, Serialize};

use crate::transport::{TransportError, UniRecv};

/// A device the phone may watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimViewableWire {
    /// What [`Request::SimSubscribe`](crate::proto::Request::SimSubscribe) names.
    pub device: String,
    /// The device's own name (`iPhone 17`, `Pixel 9`).
    pub name: String,
    /// The worktree it is attached to, as the desktop labels it.
    pub worktree: String,
    pub class: SimViewClassWire,
    /// Streaming on the desktop now. A device that is not cannot be watched
    /// until it is (the phone never boots or wakes one).
    pub streaming: bool,
}

/// What kind of device: a real phone's screen can show what a simulator's
/// cannot (notifications, codes), so the phone and the desktop's ask say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimViewClassWire {
    IosSimulator,
    AndroidEmulator,
    Iphone,
    AndroidPhone,
}

impl SimViewClassWire {
    /// A real device, not a simulator or emulator.
    pub fn is_physical(self) -> bool {
        matches!(self, Self::Iphone | Self::AndroidPhone)
    }
}

/// A subscription's token: names its view stream (the stream's first bytes)
/// and [`Request::SimUnsubscribe`](crate::proto::Request::SimUnsubscribe).
pub type SimViewToken = [u8; 16];

/// Reply to [`Request::SimSubscribe`](crate::proto::Request::SimSubscribe):
/// the stream is on its way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimSubscribedWire {
    pub token: SimViewToken,
    /// The picture's size in pixels now, as the device shows it. A rotation
    /// later brings a new config and frames of the new shape.
    pub width: u32,
    pub height: u32,
}

/// A view-stream frame's kind byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SimViewKind {
    Config = 0,
    Key = 1,
    Delta = 2,
}

impl SimViewKind {
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Config,
            1 => Self::Key,
            2 => Self::Delta,
            _ => return None,
        })
    }
}

/// The largest frame payload a view stream carries. A key frame of a full
/// resolution phone screen is well under this; anything larger is a broken
/// or hostile stream, refused before allocating it.
pub const MAX_VIEW_PAYLOAD: usize = 4 * 1024 * 1024;

/// Bytes of a frame header: length, kind, pts.
pub const VIEW_HEADER: usize = 4 + 1 + 8;

/// One frame's header and payload, ready to write.
pub fn encode_view_frame(kind: SimViewKind, pts_us: u64, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(1 + 8 + payload.len()).expect("a view payload is capped far below 4 GiB");
    let mut out = Vec::with_capacity(VIEW_HEADER + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind as u8);
    out.extend_from_slice(&pts_us.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A frame read off a view stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimViewFrame {
    pub kind: SimViewKind,
    pub pts_us: u64,
    pub payload: Vec<u8>,
}

/// Why a view stream stopped making sense.
#[derive(Debug, thiserror::Error)]
pub enum ViewStreamError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("the view stream ended mid-frame")]
    Truncated,
    #[error("a view frame of {0} bytes, over the limit")]
    TooLarge(usize),
    #[error("a view frame of unknown kind {0}")]
    UnknownKind(u8),
}

/// Fill `buf` from `stream`. `Ok(false)` when the stream ended cleanly before
/// the first byte; ending part way is [`ViewStreamError::Truncated`].
async fn read_exact(stream: &mut dyn UniRecv, buf: &mut [u8]) -> Result<bool, ViewStreamError> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]).await? {
            Some(0) => continue,
            Some(n) => filled += n,
            None if filled == 0 => return Ok(false),
            None => return Err(ViewStreamError::Truncated),
        }
    }
    Ok(true)
}

/// Read a view stream's token. `Ok(None)` when it ended before one.
pub async fn read_view_token(stream: &mut dyn UniRecv) -> Result<Option<SimViewToken>, ViewStreamError> {
    let mut token = [0u8; 16];
    Ok(read_exact(stream, &mut token).await?.then_some(token))
}

/// Read the next frame. `Ok(None)` when the stream ended on a frame boundary.
pub async fn read_view_frame(stream: &mut dyn UniRecv) -> Result<Option<SimViewFrame>, ViewStreamError> {
    let mut header = [0u8; VIEW_HEADER];
    if !read_exact(stream, &mut header).await? {
        return Ok(None);
    }
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let Some(payload_len) = len.checked_sub(9) else {
        return Err(ViewStreamError::Truncated);
    };
    if payload_len > MAX_VIEW_PAYLOAD {
        return Err(ViewStreamError::TooLarge(payload_len));
    }
    let kind = SimViewKind::from_byte(header[4]).ok_or(ViewStreamError::UnknownKind(header[4]))?;
    let pts_us = u64::from_be_bytes(header[5..13].try_into().expect("8 bytes"));
    let mut payload = vec![0u8; payload_len];
    if !read_exact(stream, &mut payload).await? && payload_len > 0 {
        return Err(ViewStreamError::Truncated);
    }
    Ok(Some(SimViewFrame { kind, pts_us, payload }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::executor::block_on;

    /// A uni-stream over a byte buffer, read in small pieces so frames
    /// straddle reads.
    struct Bytes(Vec<u8>, usize);

    #[async_trait]
    impl UniRecv for Bytes {
        async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TransportError> {
            if self.1 == self.0.len() {
                return Ok(None);
            }
            let n = buf.len().min(3).min(self.0.len() - self.1);
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(Some(n))
        }
    }

    #[test]
    fn frames_round_trip_after_the_token() {
        let token = [7u8; 16];
        let mut wire = token.to_vec();
        wire.extend(encode_view_frame(SimViewKind::Config, 0, &[1, 0x64, 0, 0x1f]));
        wire.extend(encode_view_frame(SimViewKind::Key, 66_666, &[0, 0, 0, 2, 0x65, 0x88]));
        wire.extend(encode_view_frame(SimViewKind::Delta, 133_333, &[]));
        let mut stream = Bytes(wire, 0);
        block_on(async {
            assert_eq!(read_view_token(&mut stream).await.unwrap(), Some(token));
            let config = read_view_frame(&mut stream).await.unwrap().unwrap();
            assert_eq!((config.kind, config.pts_us, config.payload), (SimViewKind::Config, 0, vec![1, 0x64, 0, 0x1f]));
            let key = read_view_frame(&mut stream).await.unwrap().unwrap();
            assert_eq!((key.kind, key.pts_us), (SimViewKind::Key, 66_666));
            let empty = read_view_frame(&mut stream).await.unwrap().unwrap();
            assert_eq!((empty.kind, empty.payload.len()), (SimViewKind::Delta, 0));
            assert!(read_view_frame(&mut stream).await.unwrap().is_none(), "clean end");
        });
    }

    #[test]
    fn a_broken_stream_is_refused_not_trusted() {
        block_on(async {
            let mut cut = encode_view_frame(SimViewKind::Key, 1, &[1, 2, 3, 4]);
            cut.truncate(cut.len() - 1);
            assert!(matches!(read_view_frame(&mut Bytes(cut, 0)).await, Err(ViewStreamError::Truncated)));
            let mut huge = ((MAX_VIEW_PAYLOAD + 10) as u32).to_be_bytes().to_vec();
            huge.extend([1; 9]);
            assert!(matches!(read_view_frame(&mut Bytes(huge, 0)).await, Err(ViewStreamError::TooLarge(_))));
            let mut odd = encode_view_frame(SimViewKind::Key, 1, &[]);
            odd[4] = 9;
            assert!(matches!(read_view_frame(&mut Bytes(odd, 0)).await, Err(ViewStreamError::UnknownKind(9))));
            let short = 3u32.to_be_bytes().iter().copied().chain([0; 9]).collect();
            assert!(matches!(read_view_frame(&mut Bytes(short, 0)).await, Err(ViewStreamError::Truncated)));
            assert_eq!(read_view_token(&mut Bytes(vec![1, 2], 0)).await.ok(), None, "a cut token");
        });
    }
}
