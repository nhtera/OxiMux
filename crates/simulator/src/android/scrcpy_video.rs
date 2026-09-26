//! The scrcpy 4.1 video socket, device → client (`doc/develop.md` §Protocol,
//! `Streamer.java`):
//!
//! 1. On the first socket of a forward tunnel: one dummy byte as soon as it
//!    is accepted (proof the server is really there — adb accepts the
//!    connection either way), then, once *every* socket is connected, the
//!    device name in a 64-byte NUL-padded field.
//! 2. The codec id (`u32`); `0` and `1` mean the device disabled the stream
//!    (1: a configuration error).
//! 3. Then packets, each with a 12-byte header whose top bit says which:
//!    - a **session** packet (bit set): video width and height, sent for each
//!      capture session (a rotation starts a new one);
//!    - a **media** packet: config / key-frame flags and a 61-bit PTS, the
//!      payload size, then the payload (H.264 Annex-B from `MediaCodec`).

use std::io::Read;

use crate::{Result, SimError};

/// `"h264"`.
pub const CODEC_H264: u32 = 0x6832_3634;
/// `DEVICE_NAME_FIELD_LENGTH`.
pub const DEVICE_NAME_LEN: usize = 64;
/// A packet larger than this is a desync, not a frame.
const MAX_PACKET: u32 = 32 * 1024 * 1024;

const SESSION_FLAG: u64 = 1 << 63;
const CONFIG_FLAG: u64 = 1 << 62;
const KEY_FRAME_FLAG: u64 = 1 << 61;
const PTS_MASK: u64 = KEY_FRAME_FLAG - 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoEvent {
    /// A capture session began at this frame size.
    Session { width: u32, height: u32, client_resized: bool },
    Media(MediaPacket),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
    /// Codec configuration (SPS/PPS), not a picture.
    pub config: bool,
    pub key_frame: bool,
    /// Microseconds; 0 for a config packet.
    pub pts: u64,
    pub data: Vec<u8>,
}

/// Read the dummy byte a forward tunnel's first socket gets on accept.
pub fn read_dummy_byte(r: &mut impl Read) -> Result<()> {
    let mut dummy = [0u8; 1];
    r.read_exact(&mut dummy).map_err(|e| eof("the scrcpy server did not answer", e))
}

/// Read the device name, which follows once the control socket is connected
/// too (the server sends it only after accepting every socket).
pub fn read_device_name(r: &mut impl Read) -> Result<String> {
    let mut name = [0u8; DEVICE_NAME_LEN];
    r.read_exact(&mut name).map_err(|e| eof("device name", e))?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    Ok(String::from_utf8_lossy(&name[..end]).into_owned())
}

/// Read the codec id; an error if the device disabled the stream or it is not
/// H.264 (the only codec we request).
pub fn read_codec(r: &mut impl Read) -> Result<u32> {
    let codec = read_u32(r).map_err(|e| eof("codec id", e))?;
    match codec {
        CODEC_H264 => Ok(codec),
        0 => Err(SimError::HelperFailed("the device disabled the video stream".into())),
        1 => Err(SimError::HelperFailed("the device could not configure video capture".into())),
        other => Err(SimError::Protocol(format!("unexpected video codec {other:#010x}"))),
    }
}

/// Read one packet. Blocking; an `Io` error when the socket closes.
pub fn read_event(r: &mut impl Read) -> Result<VideoEvent> {
    let mut header = [0u8; 12];
    r.read_exact(&mut header)?;
    let first = u64::from_be_bytes(header[..8].try_into().expect("8 bytes"));
    if first & SESSION_FLAG != 0 {
        return Ok(VideoEvent::Session {
            width: u32::from_be_bytes(header[4..8].try_into().expect("4 bytes")),
            height: u32::from_be_bytes(header[8..12].try_into().expect("4 bytes")),
            client_resized: header[3] & 1 == 1,
        });
    }
    let size = u32::from_be_bytes(header[8..12].try_into().expect("4 bytes"));
    if size > MAX_PACKET {
        return Err(SimError::Protocol(format!("video packet of {size} bytes")));
    }
    let mut data = vec![0u8; size as usize];
    r.read_exact(&mut data)?;
    let config = first & CONFIG_FLAG != 0;
    Ok(VideoEvent::Media(MediaPacket {
        config,
        key_frame: !config && first & KEY_FRAME_FLAG != 0,
        pts: if config { 0 } else { first & PTS_MASK },
        data,
    }))
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn eof(what: &str, e: std::io::Error) -> SimError {
    SimError::Protocol(format!("{what}: {e}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// A session packet as `Streamer.writeSessionMeta` writes it.
    pub(crate) fn session(width: u32, height: u32, resized: bool) -> Vec<u8> {
        let mut b = (0x8000_0000u32 | u32::from(resized)).to_be_bytes().to_vec();
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b
    }

    /// A media packet as `Streamer.writeFrameMeta` + payload write it.
    pub(crate) fn media(config: bool, key: bool, pts: u64, data: &[u8]) -> Vec<u8> {
        let flags = if config { CONFIG_FLAG } else { pts | if key { KEY_FRAME_FLAG } else { 0 } };
        let mut b = flags.to_be_bytes().to_vec();
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn a_stream_reads_preamble_codec_then_packets() {
        let mut bytes = vec![0u8];
        let mut name = [0u8; DEVICE_NAME_LEN];
        name[..14].copy_from_slice(b"sdk_gphone64_a");
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(&CODEC_H264.to_be_bytes());
        bytes.extend(session(1080, 2400, false));
        bytes.extend(media(true, false, 0, &[0, 0, 0, 1, 0x67, 0x42]));
        bytes.extend(media(false, true, 1_000_000, &[0, 0, 0, 1, 0x65, 0xaa]));
        bytes.extend(media(false, false, 1_033_333, &[0, 0, 0, 1, 0x41]));
        bytes.extend(session(2400, 1080, true));
        let mut r = Cursor::new(bytes);

        read_dummy_byte(&mut r).unwrap();
        assert_eq!(read_device_name(&mut r).unwrap(), "sdk_gphone64_a");
        assert_eq!(read_codec(&mut r).unwrap(), CODEC_H264);
        assert_eq!(read_event(&mut r).unwrap(), VideoEvent::Session { width: 1080, height: 2400, client_resized: false });
        let VideoEvent::Media(config) = read_event(&mut r).unwrap() else { panic!("media") };
        assert!(config.config && !config.key_frame && config.pts == 0);
        let VideoEvent::Media(key) = read_event(&mut r).unwrap() else { panic!("media") };
        assert!(!key.config && key.key_frame);
        assert_eq!((key.pts, key.data.as_slice()), (1_000_000, &[0, 0, 0, 1, 0x65, 0xaa][..]));
        let VideoEvent::Media(delta) = read_event(&mut r).unwrap() else { panic!("media") };
        assert!(!delta.key_frame && delta.pts == 1_033_333);
        assert_eq!(read_event(&mut r).unwrap(), VideoEvent::Session { width: 2400, height: 1080, client_resized: true });
        assert!(read_event(&mut r).is_err(), "end of stream");
    }

    #[test]
    fn a_disabled_or_foreign_stream_is_an_error() {
        for (codec, needle) in [(0u32, "disabled"), (1, "configure"), (0x0061_7631, "unexpected")] {
            let err = read_codec(&mut Cursor::new(codec.to_be_bytes())).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        }
        assert!(read_dummy_byte(&mut Cursor::new(Vec::new())).is_err(), "no server behind the forward");
    }

    #[test]
    fn an_absurd_packet_size_is_a_desync() {
        let mut header = 5u64.to_be_bytes().to_vec();
        header.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(read_event(&mut Cursor::new(header)), Err(SimError::Protocol(_))));
    }
}
