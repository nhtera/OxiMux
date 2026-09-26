//! The stdio wire protocol spoken with `oximux-sim-helper`, version 2 (1 is
//! still accepted: it has no `video`).
//!
//! The spec lives with the helper, in the fork: `oximux/PROTOCOL.md` in
//! `nhtera/serve-sim` (branch `oximux`). This module is its Rust half and must
//! change in lockstep; `tests/conformance.rs` checks it against the shipped
//! binary's `--conformance` mode.
//!
//! - Outbound (helper → us, its stdout): `[u8 kind][u32 LE len][payload]`;
//!   kind 1 is a frame `[u32 LE w][u32 LE h][JPEG]`, kind 2 a JSON event,
//!   kind 3 an H.264 picture `[u32 LE w][u32 LE h][u8 tag][AVCC]`.
//! - Inbound (us → helper, its stdin): `[u32 LE len][JSON command]`.
//!
//! Reading is bounded: a length prefix over [`MAX_MESSAGE_BYTES`] is refused
//! before anything is allocated, so a corrupt or hostile stream cannot make us
//! reserve gigabytes.

use std::io::Read;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{Button, Orientation, Result, SimError};

/// The protocol version this build speaks. The helper announces its own in
/// the first `hello` event; one outside [`MIN_PROTOCOL_VERSION`]`..=` this is
/// [`SimError::HelperIncompatible`].
pub const PROTOCOL_VERSION: u32 = 2;

/// The oldest helper protocol this build still drives. Version 1 streams JPEG
/// only; [`StreamFormat::Avcc`] needs [`AVCC_MIN_VERSION`].
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// The first protocol version with `--format avcc` and `video` messages.
pub const AVCC_MIN_VERSION: u32 = 2;

/// Largest outbound message we accept (a full-resolution iPad JPEG is a few
/// MB; 16 MiB leaves room without letting a bad prefix run away).
pub const MAX_MESSAGE_BYTES: u32 = 16 << 20;

/// Largest inbound command the helper accepts. Every [`Command`] is a few
/// fixed fields with no free-form strings, so ours stay far below it.
pub const MAX_COMMAND_BYTES: usize = 1 << 20;

const KIND_FRAME: u8 = 1;
const KIND_EVENT: u8 = 2;
const KIND_VIDEO: u8 = 3;

/// One JPEG frame, already scaled and rotated for display by the helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// The stream encoding asked of the helper (`--format`, `configure.format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat {
    /// One JPEG [`Frame`] per picture.
    #[default]
    Jpeg,
    /// H.264 [`Video`] messages.
    Avcc,
}

impl StreamFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Avcc => "avcc",
        }
    }
}

/// What a [`Video`] message carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTag {
    /// The avcC record (SPS/PPS); the helper sends one before every key frame.
    Description,
    /// An IDR picture.
    Keyframe,
    /// A picture that depends on the ones before it.
    Delta,
}

/// One H.264 message. `width`/`height` are the display size (scaled and
/// rotated), like a [`Frame`]'s; pictures are AVCC (4-byte big-endian NAL
/// lengths).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub tag: VideoTag,
    pub data: Vec<u8>,
}

/// Why the helper gave up at startup (`fatal.reason`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FatalReason {
    BadArgs,
    FrameworkLoadFailed,
    DeviceNotFound,
    DeviceNotBooted,
    CaptureFailed,
    Other(String),
}

impl FatalReason {
    fn parse(s: &str) -> Self {
        match s {
            "bad_args" => Self::BadArgs,
            "framework_load_failed" => Self::FrameworkLoadFailed,
            "device_not_found" => Self::DeviceNotFound,
            "device_not_booted" => Self::DeviceNotBooted,
            "capture_failed" => Self::CaptureFailed,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// A JSON event from the helper.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Always the first message.
    Hello { proto: u32, version: String, xcode: Option<String> },
    /// Capture is running; precedes every `Size` and frame.
    Ready { udid: String, pid: u32, orientation: Option<Orientation> },
    /// The framebuffer size in pixels: always portrait, unscaled.
    Size { width: u32, height: u32 },
    /// A commanded orientation took effect; following frames are rotated.
    Orientation(Orientation),
    /// The reply to a command that carried `id`.
    Response { id: u64, result: std::result::Result<Value, String> },
    /// A non-fatal problem with no id to answer.
    Error { message: String },
    /// The helper changed the stream format on its own (H.264 kept failing);
    /// `format` is what it streams now, when this build knows the name.
    Format { format: Option<StreamFormat>, message: String },
    /// Startup failed; the helper exits right after.
    Fatal { reason: FatalReason, message: String },
    /// `--conformance` only: how the helper parsed a command.
    Parsed { id: Option<u64>, command: std::result::Result<Value, String> },
    /// `--conformance` only: the fixed opening sequence is done.
    ConformanceReady,
    /// An event this build does not know (a newer helper); kept for logs.
    Unknown(Value),
    /// A well-framed event body that did not decode. Only broken *framing*
    /// ends a stream; one bad event is reported and skipped.
    Malformed(String),
}

/// One outbound message.
#[derive(Clone, Debug, PartialEq)]
pub enum Outbound {
    Frame(Frame),
    Video(Video),
    Event(Event),
}

/// Read one message from the helper's stdout. `Ok(None)` is a clean EOF at a
/// message boundary; EOF mid-message is an error, as is an oversized length.
pub fn read_message(r: &mut impl Read) -> Result<Option<Outbound>> {
    let mut header = [0u8; 5];
    match read_exact_or_eof(r, &mut header)? {
        Fill::Eof => return Ok(None),
        Fill::Full => {}
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    if len > MAX_MESSAGE_BYTES {
        return Err(SimError::Protocol(format!("message of {len} bytes exceeds {MAX_MESSAGE_BYTES}")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).map_err(truncated)?;
    match header[0] {
        KIND_FRAME => decode_frame(payload).map(|f| Some(Outbound::Frame(f))),
        KIND_VIDEO => decode_video(payload).map(|v| Some(Outbound::Video(v))),
        KIND_EVENT => Ok(Some(Outbound::Event(
            decode_event(&payload).unwrap_or_else(|e| Event::Malformed(e.to_string())),
        ))),
        kind => Err(SimError::Protocol(format!("unknown message kind {kind}"))),
    }
}

enum Fill {
    Full,
    Eof,
}

/// Like `read_exact`, but distinguishes EOF before the first byte (a clean
/// end) from EOF part-way through (a truncated message).
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> Result<Fill> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(Fill::Eof),
            Ok(0) => return Err(truncated(std::io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Fill::Full)
}

fn truncated(e: std::io::Error) -> SimError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        SimError::Protocol("helper output ended mid-message".into())
    } else {
        SimError::Io(e)
    }
}

fn decode_frame(mut payload: Vec<u8>) -> Result<Frame> {
    if payload.len() < 8 {
        return Err(SimError::Protocol(format!("frame payload of {} bytes has no header", payload.len())));
    }
    let width = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let height = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    payload.drain(..8);
    Ok(Frame { width, height, jpeg: payload })
}

fn decode_video(mut payload: Vec<u8>) -> Result<Video> {
    if payload.len() < 9 {
        return Err(SimError::Protocol(format!("video payload of {} bytes has no header", payload.len())));
    }
    let width = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let height = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let tag = match payload[8] {
        1 => VideoTag::Description,
        2 => VideoTag::Keyframe,
        3 => VideoTag::Delta,
        t => return Err(SimError::Protocol(format!("unknown video tag {t}"))),
    };
    payload.drain(..9);
    Ok(Video { width, height, tag, data: payload })
}

/// Decode one event body. Unknown event names decode to [`Event::Unknown`]
/// rather than failing, so an older OxiMux tolerates a newer helper's extras.
pub fn decode_event(body: &[u8]) -> Result<Event> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| SimError::Protocol(format!("event is not JSON: {e}")))?;
    let Value::Object(map) = value else {
        return Err(SimError::Protocol("event is not a JSON object".into()));
    };
    let name = map.get("event").and_then(Value::as_str).unwrap_or_default().to_owned();
    let bad = |field: &str| SimError::Protocol(format!("`{name}` event has a bad `{field}`"));
    let event = match name.as_str() {
        "hello" => Event::Hello {
            proto: u32_field(&map, "proto").ok_or_else(|| bad("proto"))?,
            version: str_field(&map, "version").unwrap_or_default(),
            xcode: str_field(&map, "xcode"),
        },
        "ready" => Event::Ready {
            udid: str_field(&map, "udid").unwrap_or_default(),
            pid: u32_field(&map, "pid").unwrap_or(0),
            orientation: u32_field(&map, "orientation")
                .and_then(|o| u8::try_from(o).ok())
                .and_then(Orientation::from_u8),
        },
        "size" => Event::Size {
            width: u32_field(&map, "width").ok_or_else(|| bad("width"))?,
            height: u32_field(&map, "height").ok_or_else(|| bad("height"))?,
        },
        "orientation" => Event::Orientation(
            u32_field(&map, "value")
                .and_then(|o| u8::try_from(o).ok())
                .and_then(Orientation::from_u8)
                .ok_or_else(|| bad("value"))?,
        ),
        "response" => {
            let id = map.get("id").and_then(Value::as_u64).ok_or_else(|| bad("id"))?;
            let result = if map.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(map.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(str_field(&map, "error").unwrap_or_else(|| "unknown error".into()))
            };
            Event::Response { id, result }
        }
        "error" => Event::Error { message: str_field(&map, "message").unwrap_or_default() },
        "format" => Event::Format {
            format: match str_field(&map, "value").as_deref() {
                Some("jpeg") => Some(StreamFormat::Jpeg),
                Some("avcc") => Some(StreamFormat::Avcc),
                _ => None,
            },
            message: str_field(&map, "message").unwrap_or_default(),
        },
        "fatal" => Event::Fatal {
            reason: FatalReason::parse(&str_field(&map, "reason").unwrap_or_default()),
            message: str_field(&map, "message").unwrap_or_default(),
        },
        "parsed" => Event::Parsed {
            id: map.get("id").and_then(Value::as_u64),
            command: match map.get("command") {
                Some(c) => Ok(c.clone()),
                None => Err(str_field(&map, "error").unwrap_or_default()),
            },
        },
        "conformance_ready" => Event::ConformanceReady,
        _ => Event::Unknown(Value::Object(map)),
    };
    Ok(event)
}

fn str_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u32_field(map: &Map<String, Value>, key: &str) -> Option<u32> {
    map.get(key).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok())
}

/// A touch's phase. The helper maps `begin`/`move` to finger-down and `end`
/// to finger-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TouchPhase {
    Begin,
    Move,
    End,
}

/// A key transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyPhase {
    Down,
    Up,
}

/// A command for the helper. Coordinates are portrait-normalized (0..1 in the
/// portrait framebuffer) in every orientation — map display points with
/// [`crate::geometry`] first.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Touch {
        phase: TouchPhase,
        x: f64,
        y: f64,
        /// Non-zero for a touch that starts on a screen edge (the home
        /// indicator swipe); see `geometry`.
        #[serde(skip_serializing_if = "is_zero")]
        edge: u32,
    },
    Multitouch { phase: TouchPhase, x1: f64, y1: f64, x2: f64, y2: f64 },
    Scroll {
        dx: f64,
        dy: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
    },
    Key { phase: KeyPhase, usage: u32 },
    Button { name: Button },
    Configure {
        #[serde(skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fps: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", serialize_with = "orientation_number")]
        orientation: Option<Orientation>,
        /// Protocol 2+ only: an older helper rejects the whole command.
        #[serde(skip_serializing_if = "Option::is_none")]
        format: Option<StreamFormat>,
    },
    Pause,
    Resume,
    Screenshot,
    AxDescribe,
    AxFrontmost,
    MemoryWarning,
}

impl Command {
    /// Whether the helper answers this command with a `response` (so the
    /// caller should send it with an id and wait).
    pub fn expects_reply(&self) -> bool {
        matches!(
            self,
            Self::Ping
                | Self::Configure { .. }
                | Self::Screenshot
                | Self::AxDescribe
                | Self::AxFrontmost
                | Self::MemoryWarning
        )
    }

    /// The JSON body, with `id` when given.
    pub fn to_json(&self, id: Option<u64>) -> Value {
        let mut value = serde_json::to_value(self).expect("commands always serialize");
        if let (Some(id), Value::Object(map)) = (id, &mut value) {
            map.insert("id".into(), id.into());
        }
        value
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn orientation_number<S: serde::Serializer>(o: &Option<Orientation>, s: S) -> std::result::Result<S::Ok, S::Error> {
    match o {
        Some(o) => s.serialize_u8(o.as_u8()),
        None => s.serialize_none(),
    }
}

/// Frame one command for the helper's stdin: `[u32 LE len][JSON]`.
pub fn encode_command(command: &Command, id: Option<u64>) -> Vec<u8> {
    let body = serde_json::to_vec(&command.to_json(id)).expect("commands always serialize");
    debug_assert!(body.len() <= MAX_COMMAND_BYTES);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_msg(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn event_msg(json: &str) -> Vec<u8> {
        frame_msg(KIND_EVENT, json.as_bytes())
    }

    /// A reader that hands out at most `step` bytes per call, to prove the
    /// decoder never assumes a whole message arrives in one read.
    struct Chunked<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn stream() -> Vec<u8> {
        let mut bytes = event_msg(r#"{"event":"hello","proto":1,"version":"0.2.0","xcode":"/X"}"#);
        let mut payload = Vec::new();
        payload.extend_from_slice(&603u32.to_le_bytes());
        payload.extend_from_slice(&1311u32.to_le_bytes());
        payload.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xD9]);
        bytes.extend(frame_msg(KIND_FRAME, &payload));
        bytes.extend(event_msg(r#"{"event":"response","id":7,"ok":true,"result":{"pong":true}}"#));
        bytes.extend(frame_msg(KIND_VIDEO, &video_payload(2, &[0, 0, 0, 1, 0x65])));
        bytes
    }

    fn video_payload(tag: u8, data: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&603u32.to_le_bytes());
        payload.extend_from_slice(&1311u32.to_le_bytes());
        payload.push(tag);
        payload.extend_from_slice(data);
        payload
    }

    #[test]
    fn decodes_the_same_messages_at_every_chunk_size() {
        let bytes = stream();
        for step in 1..=bytes.len() {
            let mut r = Chunked { data: &bytes, step };
            let mut got = Vec::new();
            while let Some(m) = read_message(&mut r).unwrap() {
                got.push(m);
            }
            assert_eq!(got.len(), 4, "step {step}");
            assert_eq!(
                got[1],
                Outbound::Frame(Frame { width: 603, height: 1311, jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9] })
            );
            assert_eq!(
                got[2],
                Outbound::Event(Event::Response { id: 7, result: Ok(serde_json::json!({"pong": true})) })
            );
            assert_eq!(
                got[3],
                Outbound::Video(Video { width: 603, height: 1311, tag: VideoTag::Keyframe, data: vec![0, 0, 0, 1, 0x65] })
            );
        }
    }

    #[test]
    fn truncation_anywhere_is_an_error_not_a_clean_eof() {
        let bytes = stream();
        let first_len = event_msg(r#"{"event":"hello","proto":1,"version":"0.2.0","xcode":"/X"}"#).len();
        for cut in 1..bytes.len() {
            let mut r = Chunked { data: &bytes[..cut], step: 7 };
            let mut result = Ok(());
            loop {
                match read_message(&mut r) {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            // Cutting exactly at a message boundary is a clean EOF.
            let last_len = frame_msg(KIND_VIDEO, &video_payload(2, &[0, 0, 0, 1, 0x65])).len();
            let response_len = event_msg(r#"{"event":"response","id":7,"ok":true,"result":{"pong":true}}"#).len();
            let boundary = cut == first_len || cut == bytes.len() - last_len || cut == bytes.len() - last_len - response_len;
            assert_eq!(result.is_ok(), boundary, "cut at {cut}");
        }
    }

    #[test]
    fn oversized_length_is_refused_before_allocating() {
        let mut bytes = vec![KIND_FRAME];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = read_message(&mut bytes.as_slice()).unwrap_err();
        assert!(matches!(err, SimError::Protocol(ref m) if m.contains("exceeds")), "{err:?}");
    }

    #[test]
    fn a_bad_event_body_is_reported_not_fatal() {
        let mut bytes = event_msg("{not json");
        bytes.extend(event_msg(r#"{"event":"size","width":1,"height":2}"#));
        let mut r = bytes.as_slice();
        assert!(matches!(read_message(&mut r).unwrap(), Some(Outbound::Event(Event::Malformed(_)))));
        assert_eq!(read_message(&mut r).unwrap(), Some(Outbound::Event(Event::Size { width: 1, height: 2 })));
    }

    #[test]
    fn unknown_kind_and_short_frame_are_protocol_errors() {
        assert!(read_message(&mut frame_msg(9, b"x").as_slice()).is_err());
        assert!(read_message(&mut frame_msg(KIND_FRAME, &[1, 2, 3]).as_slice()).is_err());
        assert!(read_message(&mut frame_msg(KIND_VIDEO, &[0; 8]).as_slice()).is_err());
        assert!(read_message(&mut frame_msg(KIND_VIDEO, &video_payload(4, &[])).as_slice()).is_err());
        let description = read_message(&mut frame_msg(KIND_VIDEO, &video_payload(1, &[1, 0x64])).as_slice()).unwrap();
        assert!(matches!(description, Some(Outbound::Video(Video { tag: VideoTag::Description, .. }))));
        let delta = read_message(&mut frame_msg(KIND_VIDEO, &video_payload(3, &[])).as_slice()).unwrap();
        assert!(matches!(delta, Some(Outbound::Video(Video { tag: VideoTag::Delta, .. }))));
    }

    #[test]
    fn decodes_every_event_shape() {
        let cases = [
            (r#"{"event":"ready","udid":"U","pid":42,"orientation":3}"#,
             Event::Ready { udid: "U".into(), pid: 42, orientation: Some(Orientation::LandscapeLeft) }),
            (r#"{"event":"size","width":1206,"height":2622}"#, Event::Size { width: 1206, height: 2622 }),
            (r#"{"event":"orientation","value":4}"#, Event::Orientation(Orientation::LandscapeRight)),
            (r#"{"event":"response","id":3,"ok":false,"error":"nope"}"#,
             Event::Response { id: 3, result: Err("nope".into()) }),
            (r#"{"event":"error","message":"m"}"#, Event::Error { message: "m".into() }),
            (r#"{"event":"format","value":"jpeg","message":"m"}"#,
             Event::Format { format: Some(StreamFormat::Jpeg), message: "m".into() }),
            (r#"{"event":"fatal","reason":"device_not_booted","message":"m"}"#,
             Event::Fatal { reason: FatalReason::DeviceNotBooted, message: "m".into() }),
            (r#"{"event":"parsed","error":"bad"}"#, Event::Parsed { id: None, command: Err("bad".into()) }),
            (r#"{"event":"conformance_ready"}"#, Event::ConformanceReady),
        ];
        for (json, want) in cases {
            assert_eq!(decode_event(json.as_bytes()).unwrap(), want, "{json}");
        }
        assert!(matches!(decode_event(br#"{"event":"from_the_future"}"#).unwrap(), Event::Unknown(_)));
        assert!(decode_event(br#"{"event":"orientation","value":9}"#).is_err());
        assert!(decode_event(b"[1]").is_err());
    }

    #[test]
    fn commands_encode_to_the_documented_json() {
        use serde_json::json;
        let cases = [
            (Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.25, edge: 0 },
             json!({"cmd":"touch","phase":"begin","x":0.5,"y":0.25})),
            (Command::Touch { phase: TouchPhase::End, x: 1.0, y: 0.0, edge: 3 },
             json!({"cmd":"touch","phase":"end","x":1.0,"y":0.0,"edge":3})),
            (Command::Scroll { dx: 0.0, dy: -3.0, x: None, y: Some(0.5) },
             json!({"cmd":"scroll","dx":0.0,"dy":-3.0,"y":0.5})),
            (Command::Key { phase: KeyPhase::Down, usage: 0x04 }, json!({"cmd":"key","phase":"down","usage":4})),
            (Command::Button { name: Button::SideButton }, json!({"cmd":"button","name":"side_button"})),
            (Command::Configure { scale: Some(0.5), fps: None, orientation: Some(Orientation::LandscapeRight), format: None },
             json!({"cmd":"configure","scale":0.5,"orientation":4})),
            (Command::Configure { scale: None, fps: None, orientation: None, format: Some(StreamFormat::Avcc) },
             json!({"cmd":"configure","format":"avcc"})),
            (Command::AxDescribe, json!({"cmd":"ax_describe"})),
            (Command::MemoryWarning, json!({"cmd":"memory_warning"})),
        ];
        for (command, want) in cases {
            assert_eq!(command.to_json(None), want);
        }
        assert_eq!(Command::Ping.to_json(Some(9)), json!({"cmd":"ping","id":9}));
    }

    #[test]
    fn encode_command_is_length_prefixed() {
        let bytes = encode_command(&Command::Pause, Some(1));
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(&bytes[4..], br#"{"cmd":"pause","id":1}"#);
    }
}
