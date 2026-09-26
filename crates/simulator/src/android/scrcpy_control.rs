//! Control messages, client → device, on the scrcpy control socket.
//!
//! Byte-for-byte the scrcpy 4.1 client's `sc_control_msg_serialize`
//! (`app/src/control_msg.c`); the golden tests below are that project's own
//! `test_control_msg_serialize.c` vectors. Integers are big-endian.

/// `SC_CONTROL_MSG_INJECT_TEXT_MAX_LENGTH`: longer text is cut (at a UTF-8
/// boundary) — callers send long text in pieces.
pub const MAX_TEXT: usize = 300;

/// `SC_CONTROL_MSG_CLIPBOARD_TEXT_MAX_LENGTH`: 256 KiB less the header.
pub const MAX_CLIPBOARD: usize = (1 << 18) - 14;

/// A pointer id scrcpy reads as a finger on a touchscreen (`POINTER_ID_GENERIC_FINGER`).
pub const FINGER: u64 = u64::MAX - 1;
/// The second finger of a two-finger gesture (`POINTER_ID_VIRTUAL_FINGER`).
pub const SECOND_FINGER: u64 = u64::MAX - 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyAction {
    Down = 0,
    Up = 1,
}

/// `AMOTION_EVENT_ACTION_*`. The server turns a second pointer's down/up into
/// `POINTER_DOWN`/`POINTER_UP` itself; clients only send these three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MotionAction {
    Down = 0,
    Up = 1,
    Move = 2,
}

/// A point on the device, with the frame size it was measured against (the
/// server drops events whose size is not the current video size, e.g. mid
/// rotation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlMsg {
    Keycode { action: KeyAction, keycode: u32, repeat: u32, metastate: u32 },
    Text(String),
    Touch { action: MotionAction, pointer_id: u64, position: Position, pressure: f32, action_button: u32, buttons: u32 },
    /// Scroll amounts in `[-16, 16]` (clamped).
    Scroll { position: Position, hscroll: f32, vscroll: f32, buttons: u32 },
    /// Back, or wake the screen when it is off.
    BackOrScreenOn(KeyAction),
    /// Put `text` on the device clipboard and, with `paste`, paste it into the
    /// focused field — the one route for text the keymap cannot type (emoji,
    /// CJK, most accented letters). Sequence 0 asks for no acknowledgement.
    SetClipboard { sequence: u64, paste: bool, text: String },
    ExpandNotificationPanel,
    ExpandSettingsPanel,
    CollapsePanels,
    SetDisplayPower(bool),
    RotateDevice,
    /// Ask the encoder for a fresh key frame (after a hidden stream resumes).
    ResetVideo,
}

impl ControlMsg {
    fn type_id(&self) -> u8 {
        match self {
            Self::Keycode { .. } => 0,
            Self::Text(_) => 1,
            Self::Touch { .. } => 2,
            Self::Scroll { .. } => 3,
            Self::BackOrScreenOn(_) => 4,
            Self::ExpandNotificationPanel => 5,
            Self::ExpandSettingsPanel => 6,
            Self::CollapsePanels => 7,
            Self::SetClipboard { .. } => 9,
            Self::SetDisplayPower(_) => 10,
            Self::RotateDevice => 11,
            Self::ResetVideo => 17,
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = vec![self.type_id()];
        match self {
            Self::Keycode { action, keycode, repeat, metastate } => {
                buf.push(*action as u8);
                for v in [keycode, repeat, metastate] {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::Text(text) => {
                let text = truncate_utf8(text, MAX_TEXT);
                buf.extend_from_slice(&(text.len() as u32).to_be_bytes());
                buf.extend_from_slice(text.as_bytes());
            }
            Self::Touch { action, pointer_id, position, pressure, action_button, buttons } => {
                buf.push(*action as u8);
                buf.extend_from_slice(&pointer_id.to_be_bytes());
                put_position(&mut buf, position);
                buf.extend_from_slice(&unit_to_u16_fixed(*pressure).to_be_bytes());
                buf.extend_from_slice(&action_button.to_be_bytes());
                buf.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::Scroll { position, hscroll, vscroll, buttons } => {
                put_position(&mut buf, position);
                for amount in [hscroll, vscroll] {
                    let normalized = (amount / 16.0).clamp(-1.0, 1.0);
                    buf.extend_from_slice(&signed_unit_to_i16_fixed(normalized).to_be_bytes());
                }
                buf.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::SetClipboard { sequence, paste, text } => {
                buf.extend_from_slice(&sequence.to_be_bytes());
                buf.push(u8::from(*paste));
                let text = truncate_utf8(text, MAX_CLIPBOARD);
                buf.extend_from_slice(&(text.len() as u32).to_be_bytes());
                buf.extend_from_slice(text.as_bytes());
            }
            Self::BackOrScreenOn(action) => buf.push(*action as u8),
            Self::SetDisplayPower(on) => buf.push(u8::from(*on)),
            Self::ExpandNotificationPanel
            | Self::ExpandSettingsPanel
            | Self::CollapsePanels
            | Self::RotateDevice
            | Self::ResetVideo => {}
        }
        buf
    }
}

fn put_position(buf: &mut Vec<u8>, p: &Position) {
    buf.extend_from_slice(&p.x.to_be_bytes());
    buf.extend_from_slice(&p.y.to_be_bytes());
    buf.extend_from_slice(&p.width.to_be_bytes());
    buf.extend_from_slice(&p.height.to_be_bytes());
}

/// `sc_float_to_u16fp`: `[0, 1]` → `0..=0xffff`.
fn unit_to_u16_fixed(f: f32) -> u16 {
    let u = (f.clamp(0.0, 1.0) * 65536.0) as u32;
    u.min(0xffff) as u16
}

/// `sc_float_to_i16fp`: `[-1, 1]` → `-0x8000..=0x7fff`.
fn signed_unit_to_i16_fixed(f: f32) -> i16 {
    let i = (f.clamp(-1.0, 1.0) * 32768.0) as i32;
    i.clamp(-0x8000, 0x7fff) as i16
}

/// At most `max` bytes of `s`, cut at a character boundary.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FHD: (u16, u16) = (1080, 1920);

    #[test]
    fn keycode_matches_scrcpy() {
        let msg = ControlMsg::Keycode { action: KeyAction::Up, keycode: 66, repeat: 5, metastate: 0x41 };
        assert_eq!(msg.serialize(), [0, 0x01, 0, 0, 0, 0x42, 0, 0, 0, 0x05, 0, 0, 0, 0x41]);
    }

    #[test]
    fn text_matches_scrcpy_and_is_capped() {
        let mut expected = vec![1, 0, 0, 0, 0x0d];
        expected.extend_from_slice(b"hello, world!");
        assert_eq!(ControlMsg::Text("hello, world!".into()).serialize(), expected);

        let long = ControlMsg::Text("a".repeat(MAX_TEXT + 7)).serialize();
        assert_eq!(long.len(), 5 + MAX_TEXT);
        assert_eq!(&long[1..5], [0x00, 0x00, 0x01, 0x2c]);
        // Never cut inside a character.
        assert_eq!(truncate_utf8("aé", 2), "a");
    }

    #[test]
    fn touch_matches_scrcpy() {
        let msg = ControlMsg::Touch {
            action: MotionAction::Down,
            pointer_id: 0x1234567887654321,
            position: Position { x: 100, y: 200, width: FHD.0, height: FHD.1 },
            pressure: 1.0,
            action_button: 1,
            buttons: 1,
        };
        assert_eq!(
            msg.serialize(),
            [
                2, 0x00, 0x12, 0x34, 0x56, 0x78, 0x87, 0x65, 0x43, 0x21, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0xc8,
                0x04, 0x38, 0x07, 0x80, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            ]
        );
    }

    #[test]
    fn scroll_matches_scrcpy() {
        let msg = ControlMsg::Scroll {
            position: Position { x: 260, y: 1026, width: FHD.0, height: FHD.1 },
            hscroll: 16.0,
            vscroll: -16.0,
            buttons: 1,
        };
        assert_eq!(
            msg.serialize(),
            [3, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x04, 0x02, 0x04, 0x38, 0x07, 0x80, 0x7F, 0xFF, 0x80, 0x00, 0, 0, 0, 1]
        );
    }

    #[test]
    fn set_clipboard_matches_scrcpy() {
        let msg = ControlMsg::SetClipboard { sequence: 0x0102030405060708, paste: true, text: "hello, world!".into() };
        let mut expected = vec![9, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 1, 0x00, 0x00, 0x00, 0x0d];
        expected.extend_from_slice(b"hello, world!");
        assert_eq!(msg.serialize(), expected);
    }

    #[test]
    fn short_messages_match_scrcpy() {
        assert_eq!(ControlMsg::BackOrScreenOn(KeyAction::Up).serialize(), [4, 0x01]);
        assert_eq!(ControlMsg::ExpandNotificationPanel.serialize(), [5]);
        assert_eq!(ControlMsg::ExpandSettingsPanel.serialize(), [6]);
        assert_eq!(ControlMsg::CollapsePanels.serialize(), [7]);
        assert_eq!(ControlMsg::SetDisplayPower(true).serialize(), [10, 1]);
        assert_eq!(ControlMsg::RotateDevice.serialize(), [11]);
        assert_eq!(ControlMsg::ResetVideo.serialize(), [17]);
    }
}
