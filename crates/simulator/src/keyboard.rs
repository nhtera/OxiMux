//! Text → USB HID Usage Page 0x07 keyboard events (US layout), plus the
//! paste-chord fallback for anything the layout can't type directly.
//!
//! The character map and the down/down/up/up-per-character event shape
//! mirror upstream serve-sim's `text-to-keys.ts:9-119` (`US_KEYBOARD_MAP`,
//! `LEFT_SHIFT = 0xe1`, `textToKeyEvents`) verbatim — including dropping a
//! bare `\r` (`:110`) so `"\r\n"` becomes a single Enter press. Named keys and
//! the shift/paste usages come from the same table upstream draws
//! `HID_USAGE_BY_CODE` from (`packages/serve-sim/src/client/utils/hid.ts:1-28`).

/// One HID keyboard usage transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub usage: u32,
    pub down: bool,
}

impl KeyEvent {
    fn down(usage: u32) -> Self {
        Self { usage, down: true }
    }

    fn up(usage: u32) -> Self {
        Self { usage, down: false }
    }
}

/// Left Shift usage, per `hid.ts:26` (`ShiftLeft: 0xe1`) and
/// `text-to-keys.ts:7` (`LEFT_SHIFT`).
const LEFT_SHIFT: u32 = 0xe1;

/// A character outside the US keyboard map (`text_to_key_events` can't type
/// it — send it via [`needs_paste`] + [`paste_chord`] instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unsupported character: {ch:?}")]
pub struct Unsupported {
    pub ch: char,
}

/// The physical US key `(usage, shift)` for one character, or `None` when
/// it's outside the US layout's reach. Mirrors `text-to-keys.ts:12-54`
/// (`buildMap`) arm-for-arm rather than building the same table with a loop,
/// since Rust's range patterns make the loop's arithmetic just as legible as
/// a match.
fn key_spec(ch: char) -> Option<(u32, bool)> {
    match ch {
        'a'..='z' => Some((0x04 + (ch as u32 - 'a' as u32), false)),
        'A'..='Z' => Some((0x04 + (ch as u32 - 'A' as u32), true)),
        '1'..='9' => Some((0x1e + (ch as u32 - '1' as u32), false)),
        '0' => Some((0x27, false)),
        '!' => Some((0x1e, true)),
        '@' => Some((0x1f, true)),
        '#' => Some((0x20, true)),
        '$' => Some((0x21, true)),
        '%' => Some((0x22, true)),
        '^' => Some((0x23, true)),
        '&' => Some((0x24, true)),
        '*' => Some((0x25, true)),
        '(' => Some((0x26, true)),
        ')' => Some((0x27, true)),
        '-' => Some((0x2d, false)),
        '_' => Some((0x2d, true)),
        '=' => Some((0x2e, false)),
        '+' => Some((0x2e, true)),
        '[' => Some((0x2f, false)),
        '{' => Some((0x2f, true)),
        ']' => Some((0x30, false)),
        '}' => Some((0x30, true)),
        '\\' => Some((0x31, false)),
        '|' => Some((0x31, true)),
        ';' => Some((0x33, false)),
        ':' => Some((0x33, true)),
        '\'' => Some((0x34, false)),
        '"' => Some((0x34, true)),
        '`' => Some((0x35, false)),
        '~' => Some((0x35, true)),
        ',' => Some((0x36, false)),
        '<' => Some((0x36, true)),
        '.' => Some((0x37, false)),
        '>' => Some((0x37, true)),
        '/' => Some((0x38, false)),
        '?' => Some((0x38, true)),
        ' ' => Some((0x2c, false)),
        '\n' => Some((0x28, false)),
        '\t' => Some((0x2b, false)),
        _ => None,
    }
}

/// Encodes `text` as HID key events: for each character, an optional
/// Shift-down, the key down, the key up, then an optional Shift-up
/// (`text-to-keys.ts:106-119`). Fails on the first character outside the US
/// layout; callers should check [`needs_paste`] before calling this.
pub fn text_to_key_events(text: &str) -> Result<Vec<KeyEvent>, Unsupported> {
    let mut events = Vec::new();
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        let (usage, shift) = key_spec(ch).ok_or(Unsupported { ch })?;
        if shift {
            events.push(KeyEvent::down(LEFT_SHIFT));
        }
        events.push(KeyEvent::down(usage));
        events.push(KeyEvent::up(usage));
        if shift {
            events.push(KeyEvent::up(LEFT_SHIFT));
        }
    }
    Ok(events)
}

/// The usage for a named (non-printable) key, or `None` when `name` isn't
/// one of the panel's bound keys. Usages are `hid.ts`'s
/// `HID_USAGE_BY_CODE` values for the matching `KeyboardEvent.code`.
pub fn named_key(name: &str) -> Option<u32> {
    Some(match name {
        "return" => 0x28,    // hid.ts:11 Enter
        "escape" => 0x29,    // hid.ts:11 Escape
        "tab" => 0x2b,       // hid.ts:11 Tab
        "backspace" => 0x2a, // hid.ts:11 Backspace
        "delete" => 0x4c,    // hid.ts:18 Delete (forward delete)
        "up" => 0x52,        // hid.ts:19 ArrowUp
        "down" => 0x51,      // hid.ts:19 ArrowDown
        "left" => 0x50,      // hid.ts:19 ArrowLeft
        "right" => 0x4f,     // hid.ts:19 ArrowRight
        "home" => 0x4a,      // hid.ts:18 Home
        "end" => 0x4d,       // hid.ts:18 End
        "space" => 0x2c,     // hid.ts:11 Space
        _ => return None,
    })
}

/// Text at or above this length must go through [`paste_chord`] rather than
/// [`text_to_key_events`]: iOS coalesces key events that land in the same
/// tick, so a HID-per-character stream for anything long is slow and
/// unreliable compared to one clipboard paste.
pub const PASTE_THRESHOLD: usize = 4 * 1024;

/// True when `text` must be typed via [`paste_chord`] instead of
/// [`text_to_key_events`]: longer than [`PASTE_THRESHOLD`] bytes, or
/// containing a character outside the US layout (non-ASCII — the spike
/// verified Unicode text this way; see the paste note on [`paste_chord`]).
pub fn needs_paste(text: &str) -> bool {
    text.len() > PASTE_THRESHOLD || !text.is_ascii()
}

/// Left ⌘ (`0xe3`, `hid.ts:26` `MetaLeft`).
const LEFT_COMMAND: u32 = 0xe3;
/// `V` (`0x19`, `hid.ts:7` `KeyV`).
const KEY_V: u32 = 0x19;

/// The ⌘V chord, to run *after* the caller has put `text` on the guest
/// clipboard via `simctl pbcopy`. This is the paste strategy the spike
/// verified end-to-end for Unicode text (spike-report.md §2, "Unicode
/// paste": `simctl pbcopy` then HID ⌘V (0xE3 + 0x19) produced the exact
/// pasted string, no ASCII fallback needed).
pub fn paste_chord() -> Vec<KeyEvent> {
    vec![
        KeyEvent::down(LEFT_COMMAND),
        KeyEvent::down(KEY_V),
        KeyEvent::up(KEY_V),
        KeyEvent::up(LEFT_COMMAND),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(usage: u32) -> KeyEvent {
        KeyEvent::down(usage)
    }
    fn up(usage: u32) -> KeyEvent {
        KeyEvent::up(usage)
    }

    /// Pins the exact wire encoding for "Hello, World!\n" against a
    /// hand-computed expansion of upstream's `US_KEYBOARD_MAP`
    /// (`text-to-keys.ts:9-54`), so a future edit to `key_spec` that drifts
    /// from upstream fails loudly here.
    #[test]
    fn hello_world_matches_upstream_encoding() {
        let events = text_to_key_events("Hello, World!\n").unwrap();
        let expected = vec![
            down(LEFT_SHIFT), down(0x0b), up(0x0b), up(LEFT_SHIFT), // H
            down(0x08), up(0x08),                                   // e
            down(0x0f), up(0x0f),                                   // l
            down(0x0f), up(0x0f),                                   // l
            down(0x12), up(0x12),                                   // o
            down(0x36), up(0x36),                                   // ,
            down(0x2c), up(0x2c),                                   // space
            down(LEFT_SHIFT), down(0x1a), up(0x1a), up(LEFT_SHIFT), // W
            down(0x12), up(0x12),                                   // o
            down(0x15), up(0x15),                                   // r
            down(0x0f), up(0x0f),                                   // l
            down(0x07), up(0x07),                                   // d
            down(LEFT_SHIFT), down(0x1e), up(0x1e), up(LEFT_SHIFT), // !
            down(0x28), up(0x28),                                   // \n (Enter)
        ];
        assert_eq!(events, expected);
    }

    #[test]
    fn crlf_collapses_to_one_enter() {
        let events = text_to_key_events("a\r\nb").unwrap();
        assert_eq!(events, vec![down(0x04), up(0x04), down(0x28), up(0x28), down(0x05), up(0x05)]);
    }

    #[test]
    fn lone_cr_is_dropped_not_typed() {
        assert_eq!(text_to_key_events("a\rb").unwrap(), vec![down(0x04), up(0x04), down(0x05), up(0x05)]);
    }

    #[test]
    fn unsupported_character_fails_on_the_first_bad_char() {
        let err = text_to_key_events("ok 🙂 more").unwrap_err();
        assert_eq!(err, Unsupported { ch: '🙂' });
    }

    #[test]
    fn digit_zero_and_its_shifted_paren_share_a_usage() {
        assert_eq!(key_spec('0'), Some((0x27, false)));
        assert_eq!(key_spec(')'), Some((0x27, true)));
    }

    #[test]
    fn named_keys_cover_the_documented_set() {
        for name in ["return", "escape", "tab", "backspace", "delete", "up", "down", "left", "right", "home", "end", "space"] {
            assert!(named_key(name).is_some(), "missing named key {name}");
        }
        assert_eq!(named_key("pageup"), None);
    }

    #[test]
    fn needs_paste_on_length_or_non_ascii() {
        assert!(!needs_paste("short ascii"));
        assert!(needs_paste(&"a".repeat(PASTE_THRESHOLD + 1)));
        assert!(!needs_paste(&"a".repeat(PASTE_THRESHOLD)));
        assert!(needs_paste("Xin chào 👋"));
    }

    #[test]
    fn paste_chord_is_command_v_down_then_up() {
        assert_eq!(
            paste_chord(),
            vec![down(LEFT_COMMAND), down(KEY_V), up(KEY_V), up(LEFT_COMMAND)]
        );
    }
}
