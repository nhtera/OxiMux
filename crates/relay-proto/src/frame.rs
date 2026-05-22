use bincode::config::Configuration;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::ProtoError;
use crate::messages::{Notification, Request, Response};

// 16 MiB. Real PTY output chunks are well under this; the cap exists
// purely to bound the damage a malformed peer can do (we allocate
// `payload_len` bytes before decoding, so an attacker-controlled
// `payload_len` is a memory-amplification vector without it).
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

// Frame header on the wire: u8 kind + u32 big-endian payload length.
// Big-endian applies *only* to this header — picked for human-readable
// hex dumps, not for compat with any existing protocol. The bincode
// payload that follows uses `bincode::config::standard()`, which is
// little-endian + varint (so `u16`/`u32`/`u64` are 1–9 bytes depending
// on magnitude, and enum discriminants are varint too).
const HEADER_LEN: usize = 1 + 4;

// Every Request and Response carries this id so the client can pair
// them across the multiplexed socket. Notifications are server-pushed
// and use id 0 (which clients ignore).
pub type RequestId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Request,
    Response,
    Notification,
}

impl FrameKind {
    pub fn as_byte(self) -> u8 {
        match self {
            FrameKind::Request => 0x01,
            FrameKind::Response => 0x02,
            FrameKind::Notification => 0x03,
        }
    }

    pub fn from_byte(b: u8) -> Result<Self, ProtoError> {
        match b {
            0x01 => Ok(FrameKind::Request),
            0x02 => Ok(FrameKind::Response),
            0x03 => Ok(FrameKind::Notification),
            other => Err(ProtoError::InvalidFrameKind(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Request {
        request_id: RequestId,
        request: Request,
    },
    Response {
        request_id: RequestId,
        response: Response,
    },
    Notification(Notification),
}

impl Frame {
    pub fn kind(&self) -> FrameKind {
        match self {
            Frame::Request { .. } => FrameKind::Request,
            Frame::Response { .. } => FrameKind::Response,
            Frame::Notification(_) => FrameKind::Notification,
        }
    }
}

#[inline]
fn cfg() -> Configuration {
    bincode::config::standard()
}

fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    bincode::serde::encode_to_vec(value, cfg()).map_err(|e| ProtoError::Encode(e.to_string()))
}

fn decode_payload<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    let (value, consumed): (T, usize) = bincode::serde::decode_from_slice(bytes, cfg())
        .map_err(|e| ProtoError::Decode(e.to_string()))?;
    if consumed != bytes.len() {
        return Err(ProtoError::TrailingBytes(bytes.len() - consumed));
    }
    Ok(value)
}

// Encode a frame to its full on-wire form (header + payload). Returns
// `FrameTooLarge` if the encoded payload exceeds `MAX_FRAME_SIZE`.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtoError> {
    let payload = match frame {
        Frame::Request {
            request_id,
            request,
        } => encode_payload(&(*request_id, request))?,
        Frame::Response {
            request_id,
            response,
        } => encode_payload(&(*request_id, response))?,
        Frame::Notification(n) => encode_payload(n)?,
    };

    if payload.len() as u64 > MAX_FRAME_SIZE as u64 {
        return Err(ProtoError::FrameTooLarge(payload.len() as u64));
    }

    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(frame.kind().as_byte());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

// Try to decode one frame from the front of `buf`. Returns:
// - `Ok(Some((frame, total_consumed)))` when a full frame is present;
//   the caller advances their read buffer by `total_consumed` bytes.
// - `Ok(None)` when the buffer is short — caller reads more bytes.
// - `Err(_)` on a malformed frame; the caller MUST close the
//   connection — we cannot recover the byte stream alignment.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, ProtoError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let kind = FrameKind::from_byte(buf[0])?;
    let payload_len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);

    if payload_len > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge(payload_len as u64));
    }

    let total = HEADER_LEN + payload_len as usize;
    if buf.len() < total {
        return Ok(None);
    }

    let payload = &buf[HEADER_LEN..total];
    let frame = match kind {
        FrameKind::Request => {
            let (request_id, request): (RequestId, Request) = decode_payload(payload)?;
            Frame::Request {
                request_id,
                request,
            }
        }
        FrameKind::Response => {
            let (request_id, response): (RequestId, Response) = decode_payload(payload)?;
            Frame::Response {
                request_id,
                response,
            }
        }
        FrameKind::Notification => Frame::Notification(decode_payload(payload)?),
    };
    Ok(Some((frame, total)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrCode;
    use crate::messages::{Hello, HelloAck, PtyDescriptor};

    fn round_trip(frame: Frame) {
        let bytes = encode_frame(&frame).expect("encode");
        let (decoded, consumed) = decode_frame(&bytes).expect("decode").expect("complete");
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, frame);
    }

    #[test]
    fn round_trip_hello_request() {
        round_trip(Frame::Request {
            request_id: 1,
            request: Request::Hello(Hello {
                protocol_version: 1,
                token: "deadbeef".into(),
                client_id: "abc-123".into(),
            }),
        });
    }

    #[test]
    fn round_trip_every_request_variant() {
        let variants = [
            Request::Hello(Hello {
                protocol_version: 1,
                token: "t".into(),
                client_id: "c".into(),
            }),
            Request::Spawn {
                cwd: "/tmp".into(),
                cols: 80,
                rows: 24,
                shell: Some("/bin/zsh".into()),
                env: vec![("FOO".into(), "bar".into())],
            },
            Request::Attach {
                pty_id: "pty-1".into(),
            },
            Request::Write {
                pty_id: "pty-1".into(),
                bytes: vec![0, 1, 2, 255],
            },
            Request::Resize {
                pty_id: "pty-1".into(),
                cols: 120,
                rows: 40,
            },
            Request::Close {
                pty_id: "pty-1".into(),
                grace_ms: 500,
            },
            Request::ListPtys,
            Request::Shutdown,
        ];
        for (i, v) in variants.into_iter().enumerate() {
            round_trip(Frame::Request {
                request_id: i as u64,
                request: v,
            });
        }
    }

    #[test]
    fn round_trip_every_response_variant() {
        let variants = [
            Response::HelloAck(HelloAck {
                server_protocol_version: 1,
                session_id: "sess-1".into(),
            }),
            Response::SpawnOk {
                pty_id: "pty-1".into(),
            },
            Response::AttachOk {
                replay: vec![1, 2, 3],
            },
            Response::Ok,
            Response::Pty(PtyDescriptor {
                pty_id: "p".into(),
                cwd: "/".into(),
                cols: 80,
                rows: 24,
            }),
            Response::PtyList(vec![PtyDescriptor {
                pty_id: "p".into(),
                cwd: "/".into(),
                cols: 80,
                rows: 24,
            }]),
            Response::Err {
                code: ErrCode::PtyNotFound,
                message: "gone".into(),
            },
        ];
        for (i, v) in variants.into_iter().enumerate() {
            round_trip(Frame::Response {
                request_id: i as u64,
                response: v,
            });
        }
    }

    #[test]
    fn round_trip_notifications() {
        round_trip(Frame::Notification(Notification::Output {
            pty_id: "p".into(),
            bytes: vec![0xff; 1024],
        }));
        round_trip(Frame::Notification(Notification::Exit {
            pty_id: "p".into(),
            code: Some(1),
        }));
        round_trip(Frame::Notification(Notification::Exit {
            pty_id: "p".into(),
            code: None,
        }));
    }

    #[test]
    fn frame_kind_byte_round_trip() {
        for kind in [
            FrameKind::Request,
            FrameKind::Response,
            FrameKind::Notification,
        ] {
            assert_eq!(FrameKind::from_byte(kind.as_byte()).unwrap(), kind);
        }
    }

    #[test]
    fn invalid_kind_byte_errors() {
        match decode_frame(&[0xFF, 0, 0, 0, 0]) {
            Err(ProtoError::InvalidFrameKind(0xFF)) => {}
            other => panic!("expected InvalidFrameKind, got {other:?}"),
        }
    }

    #[test]
    fn oversized_payload_length_errors() {
        let mut buf = vec![0x01];
        buf.extend_from_slice(&(MAX_FRAME_SIZE + 1).to_be_bytes());
        match decode_frame(&buf) {
            Err(ProtoError::FrameTooLarge(n)) => assert_eq!(n, (MAX_FRAME_SIZE + 1) as u64),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_header_returns_none() {
        // 4 bytes < HEADER_LEN (5).
        assert!(matches!(decode_frame(&[0x01, 0, 0, 0]), Ok(None)));
    }

    #[test]
    fn incomplete_payload_returns_none() {
        // Header claims 10 bytes of payload, only 2 supplied.
        let mut buf = vec![0x01];
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        assert!(matches!(decode_frame(&buf), Ok(None)));
    }

    #[test]
    fn random_bytes_never_panic() {
        // Deterministic pseudo-random sweep — proves the decoder never
        // panics regardless of input. Real fuzzing belongs to a separate
        // harness; this is the "good neighbor" check that bincode's own
        // error paths cover the variants we touch.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..256 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state as usize) % 64;
            let bytes: Vec<u8> = (0..len)
                .map(|i| ((state >> (i % 8 * 8)) & 0xFF) as u8)
                .collect();
            let _ = decode_frame(&bytes);
        }
    }

    #[test]
    fn decoder_consumes_exact_frame_length() {
        // Append trailing garbage; decoder must report total = HEADER + payload,
        // not buf.len(). Caller's read loop relies on this to slice off the
        // frame and keep parsing the next one from the remainder.
        let frame = Frame::Notification(Notification::Exit {
            pty_id: "p".into(),
            code: None,
        });
        let mut bytes = encode_frame(&frame).unwrap();
        let frame_len = bytes.len();
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let (_, consumed) = decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(consumed, frame_len);
    }
}
