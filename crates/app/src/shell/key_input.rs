//! Keystroke → PTY-bytes translation.
//!
//! Pure function over `gpui::Keystroke`. Maps the usual xterm escape
//! sequences (cursor keys, function keys, page nav), Ctrl-letter combos,
//! Alt as ESC-prefix, and printable characters via `key_char`/`key`.
//!
//! Returning `Vec<u8>` keeps the caller decoupled from any specific PTY
//! API; the renderer is responsible for `backend.write(id, &bytes)`. An
//! empty `Vec` means "this key was consumed but produced no PTY input"
//! (currently only returned for `cmd-*` combos and bare modifier presses,
//! which belong to app-level actions, not the shell).

use gpui::Keystroke;

/// Translate a single Keystroke to the bytes that should be written to the
/// PTY. Returns an empty `Vec` when the key should not reach the shell
/// (anything with Cmd/Super pressed — those are reserved for app actions).
pub fn keystroke_to_bytes(ks: &Keystroke) -> Vec<u8> {
    // Cmd / Super combos are app-level (copy/paste/quit/etc.). Never forward.
    if ks.modifiers.platform {
        return Vec::new();
    }

    if let Some(special) = special_key(&ks.key) {
        return apply_alt(ks, special.to_vec());
    }

    // Ctrl + single ASCII letter → C0 control byte (Ctrl-A == 0x01, etc.).
    if ks.modifiers.control
        && let Some(byte) = ctrl_byte(&ks.key)
    {
        return apply_alt(ks, vec![byte]);
    }

    // Printable character. Prefer key_char (post-IME, shift-aware) over key.
    if let Some(s) = ks.key_char.as_deref().filter(|s| !s.is_empty()) {
        return apply_alt(ks, s.as_bytes().to_vec());
    }

    if !ks.key.is_empty() && is_printable_key(&ks.key) {
        return apply_alt(ks, ks.key.as_bytes().to_vec());
    }

    Vec::new()
}

/// xterm escape sequences for the named keys. None means "not special;
/// fall through to printable handling".
fn special_key(key: &str) -> Option<&'static [u8]> {
    match key {
        "enter" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        "space" => Some(b" "),
        "left" => Some(b"\x1b[D"),
        "right" => Some(b"\x1b[C"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "insert" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    }
}

/// Ctrl-A..Z maps to bytes 0x01..0x1A. Ctrl-` is 0x00, Ctrl-[ is 0x1B,
/// Ctrl-\ is 0x1C, Ctrl-] is 0x1D, Ctrl-^ is 0x1E, Ctrl-_ is 0x1F. Anything
/// else returns None and the caller falls through.
fn ctrl_byte(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' => Some(0x00),
        '[' => Some(0x1B),
        '\\' => Some(0x1C),
        ']' => Some(0x1D),
        '^' => Some(0x1E),
        '_' => Some(0x1F),
        ' ' => Some(0x00),
        _ => None,
    }
}

fn is_printable_key(key: &str) -> bool {
    // Multi-char "names" like "shift", "ctrl" arrive in `key`; one-char keys
    // are the printable case. Anything multi-char that isn't a special key
    // already returned above is a bare modifier press → ignore.
    key.chars().count() == 1
}

fn apply_alt(ks: &Keystroke, mut bytes: Vec<u8>) -> Vec<u8> {
    if ks.modifiers.alt {
        bytes.insert(0, 0x1B);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn ks(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    #[test]
    fn printable_chars() {
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("a"), Modifiers::default())),
            b"a"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("A", Some("A"), Modifiers::default())),
            b"A"
        );
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(
            keystroke_to_bytes(&ks("enter", None, Modifiers::default())),
            b"\r"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("backspace", None, Modifiers::default())),
            b"\x7f"
        );
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, Modifiers::default())),
            b"\x1b[A"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, Modifiers::default())),
            b"\x1b[C"
        );
    }

    #[test]
    fn ctrl_c_is_0x03() {
        let m = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("c", Some("c"), m)), b"\x03");
    }

    #[test]
    fn alt_a_prefixes_esc() {
        let m = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("a", Some("a"), m)), b"\x1ba");
    }

    #[test]
    fn cmd_combos_swallowed() {
        let m = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("c", Some("c"), m)), Vec::<u8>::new());
    }
}
