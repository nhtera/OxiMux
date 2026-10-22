//! Named keys → the bytes a terminal sends for them, for `oximux term send
//! --keys`. A small pure table with the same encodings the desktop terminal
//! uses (xterm: CSI arrows, SS3 under app-cursor mode, `~`-terminated
//! navigation and F5–F12), kept separate because the desktop encoder works on
//! GUI keystrokes the CLI does not have.

/// The key names `--keys` accepts, for the error message and `--help`.
pub const KEY_NAMES: &str = "enter, tab, esc, backspace, space, up, down, left, right, home, end, \
     pgup, pgdn, insert, delete, f1-f12, ctrl-<a-z>, alt-<char> (any but a comma)";

/// Encode one named key. `app_cursor` picks SS3 (`ESC O A`) over CSI
/// (`ESC [ A`) for the arrows, Home and End — what a full-screen program that
/// turned on application cursor mode expects. Names are case-insensitive.
pub fn encode_key(name: &str, app_cursor: bool) -> Result<Vec<u8>, String> {
    let lower = name.trim().to_ascii_lowercase();
    if let Some(letter) = lower.strip_prefix("ctrl-") {
        return match letter.as_bytes() {
            [c @ b'a'..=b'z'] => Ok(vec![c - b'a' + 1]),
            _ => Err(format!("`{name}`: ctrl- takes one letter a-z")),
        };
    }
    // Alt is the Meta convention: ESC, then the key. The payload keeps its
    // case (`alt-B` is ESC B), so it is read from the original spelling.
    if lower.starts_with("alt-") {
        let payload = &name.trim()[4..];
        let mut chars = payload.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(c.to_string().as_bytes());
                Ok(bytes)
            }
            _ => Err(format!("`{name}`: alt- takes one printable character")),
        };
    }
    let cursor = |final_byte: u8| {
        if app_cursor { vec![0x1b, b'O', final_byte] } else { vec![0x1b, b'[', final_byte] }
    };
    let tilde = |n: u8| format!("\x1b[{n}~").into_bytes();
    Ok(match lower.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "space" => b" ".to_vec(),
        "up" => cursor(b'A'),
        "down" => cursor(b'B'),
        "right" => cursor(b'C'),
        "left" => cursor(b'D'),
        "home" => cursor(b'H'),
        "end" => cursor(b'F'),
        "pgup" | "pageup" => tilde(5),
        "pgdn" | "pagedown" => tilde(6),
        "insert" => tilde(2),
        "delete" => tilde(3),
        // F1–F4 are SS3 in every mode; application cursor mode does not apply.
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        _ => return Err(format!("unknown key `{name}` (known: {KEY_NAMES})")),
    })
}

/// Everything `term send` types, in order: the text as given, then each named
/// key, then Enter if asked. Fails on the first unknown key.
pub fn compose(text: &[u8], keys: &[String], enter: bool, app_cursor: bool) -> Result<Vec<u8>, String> {
    let mut bytes = text.to_vec();
    for key in keys {
        bytes.extend(encode_key(key, app_cursor)?);
    }
    if enter {
        bytes.push(b'\r');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Vec<u8> {
        encode_key(name, false).unwrap()
    }

    #[test]
    fn plain_keys_encode_as_a_terminal_sends_them() {
        assert_eq!(key("enter"), b"\r");
        assert_eq!(key("Tab"), b"\t");
        assert_eq!(key("esc"), b"\x1b");
        assert_eq!(key("backspace"), b"\x7f");
        assert_eq!(key("pgup"), b"\x1b[5~");
        assert_eq!(key("pgdn"), b"\x1b[6~");
        assert_eq!(key("delete"), b"\x1b[3~");
        assert_eq!(key("f1"), b"\x1bOP");
        assert_eq!(key("f5"), b"\x1b[15~");
        assert_eq!(key("f12"), b"\x1b[24~");
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        assert_eq!(key("up"), b"\x1b[A");
        assert_eq!(key("left"), b"\x1b[D");
        assert_eq!(key("home"), b"\x1b[H");
        assert_eq!(encode_key("up", true).unwrap(), b"\x1bOA");
        assert_eq!(encode_key("end", true).unwrap(), b"\x1bOF");
        // Function keys do not change with it.
        assert_eq!(encode_key("f2", true).unwrap(), b"\x1bOQ");
        assert_eq!(encode_key("f6", true).unwrap(), b"\x1b[17~");
    }

    #[test]
    fn ctrl_and_alt_combos() {
        assert_eq!(key("ctrl-c"), [0x03]);
        assert_eq!(key("CTRL-A"), [0x01]);
        assert_eq!(key("ctrl-z"), [0x1a]);
        assert_eq!(key("alt-b"), b"\x1bb");
        assert_eq!(key("alt-B"), b"\x1bB");
        assert_eq!(key("alt-."), b"\x1b.");
    }

    #[test]
    fn compose_types_text_then_keys_then_enter() {
        let keys = vec!["left".to_string(), "ctrl-a".to_string()];
        assert_eq!(compose(b"ls", &keys, true, false).unwrap(), b"ls\x1b[D\x01\r");
        assert_eq!(compose(b"", &[], true, false).unwrap(), b"\r");
        // Text is sent exactly as given: no newline is added without --enter.
        assert_eq!(compose(b"echo hi", &[], false, false).unwrap(), b"echo hi");
        assert!(compose(b"x", &["nope".to_string()], false, false).is_err());
    }

    #[test]
    fn unknown_or_malformed_names_are_refused() {
        for bad in ["", "ctrl-", "ctrl-1", "ctrl-ab", "alt-", "alt-ab", "f13", "hyper-x"] {
            assert!(encode_key(bad, false).is_err(), "{bad:?} should be refused");
        }
    }
}
