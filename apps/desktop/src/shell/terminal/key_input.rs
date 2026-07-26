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
use oximux_pty::InputMode;

/// Translate a single Keystroke to the bytes that should be written to the
/// PTY. `mode` carries DECCKM (app-cursor) so cursor keys pick CSI vs SS3.
/// Returns an empty `Vec` when the key should not reach the shell (anything
/// with Cmd/Super pressed — those are reserved for app actions).
pub fn keystroke_to_bytes(ks: &Keystroke, mode: InputMode) -> Vec<u8> {
    // Cmd / Super combos are app-level (copy/paste/quit/etc.). Never forward.
    if ks.modifiers.platform {
        return Vec::new();
    }

    // Shift+Tab is backtab (CSI Z) — what shells map to reverse menu
    // completion. The Raw classifier below is modifier-blind and would emit a
    // plain `\t`, so the shifted form is handled explicitly here. Require no
    // Ctrl/Alt so the MRU-tab shortcuts (Ctrl[+Shift]+Tab) never reach here.
    if ks.key == "tab" && ks.modifiers.shift && !ks.modifiers.control && !ks.modifiers.alt {
        return b"\x1b[Z".to_vec();
    }

    if let Some(seq) = classify(&ks.key) {
        return encode_special(seq, ks, mode);
    }

    // Ctrl + single ASCII letter → C0 control byte (Ctrl-A == 0x01, etc.).
    if ks.modifiers.control
        && let Some(byte) = ctrl_byte(&ks.key)
    {
        return apply_alt(ks, vec![byte]);
    }

    // Alt + printable key uses the UNSHIFTED key label, not `key_char`. On
    // macOS, Option composes platform characters (Option+A → `å`) into
    // `key_char`; if we forwarded that as the Meta payload the shell would
    // see `ESC å` (3 bytes) and no readline Alt binding would fire — bindings
    // are registered against `ESC a` (the ASCII letter). When the caller
    // wants the composed character instead (the "Option-as-Meta = off"
    // setting), it strips `alt` before invoking us so this branch is skipped
    // and the `key_char` path below runs.
    if ks.modifiers.alt && !ks.key.is_empty() && is_printable_key(&ks.key) {
        return apply_alt(ks, ks.key.as_bytes().to_vec());
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

/// True when this keystroke is plain printable text that the platform input
/// method owns. Such keys are delivered to the view through the IME input
/// handler (commit + marked-text callbacks), so the key encoder must NOT also
/// translate them to PTY bytes — encoding them here as well would double-type
/// the character and bypass multi-keystroke composition (e.g. Vietnamese
/// Telex `as`→`á`, `dd`→`đ`, or any CJK input method).
///
/// Returns `false` for modified keys (Ctrl/Alt/Cmd combos) and for named
/// navigation/control keys (Enter, Tab, arrows, function keys, …), which are
/// not text and must stay on the byte-encoding path. Space is treated as text
/// (it both inserts a space and commits an in-progress composition), so its
/// ordering is left to the IME.
pub fn is_ime_text_key(ks: &Keystroke) -> bool {
    let m = &ks.modifiers;
    if m.platform || m.control || m.alt {
        return false;
    }
    if ks.key == "space" {
        return true;
    }
    classify(&ks.key).is_none() && is_printable_key(&ks.key)
}

/// Shape of a named navigation key for escape-sequence encoding.
enum KeySeq {
    /// Cursor-style with a final byte: A/B/C/D (arrows), H/F (Home/End).
    /// Unmodified → `ESC [ <final>`; app-cursor (DECCKM) → `ESC O <final>`;
    /// with any modifier → `ESC [ 1 ; <mod> <final>`.
    Cursor(u8),
    /// SS3-anchored key (F1-F4: P/Q/R/S). Unmodified → `ESC O <final>`
    /// ALWAYS (terminfo `kf1=\EOP`; DECCKM does not apply to function keys);
    /// with any modifier → `ESC [ 1 ; <mod> <final>`.
    Ss3(u8),
    /// Tilde-terminated with a numeric parameter (PageUp/Down, Insert,
    /// Delete, F5-F12). Unmodified → `ESC [ <n> ~`; modified →
    /// `ESC [ <n> ; <mod> ~`.
    Tilde(u8),
    /// Fixed bytes with no modifier encoding (enter, tab, escape, space).
    /// Alt still ESC-prefixes these (Meta convention).
    Raw(&'static [u8]),
}

/// Classify a named key. `None` falls through to printable handling.
fn classify(key: &str) -> Option<KeySeq> {
    Some(match key {
        "up" => KeySeq::Cursor(b'A'),
        "down" => KeySeq::Cursor(b'B'),
        "right" => KeySeq::Cursor(b'C'),
        "left" => KeySeq::Cursor(b'D'),
        "home" => KeySeq::Cursor(b'H'),
        "end" => KeySeq::Cursor(b'F'),
        "f1" => KeySeq::Ss3(b'P'),
        "f2" => KeySeq::Ss3(b'Q'),
        "f3" => KeySeq::Ss3(b'R'),
        "f4" => KeySeq::Ss3(b'S'),
        "enter" => KeySeq::Raw(b"\r"),
        "tab" => KeySeq::Raw(b"\t"),
        "backspace" => KeySeq::Raw(b"\x7f"),
        "escape" => KeySeq::Raw(b"\x1b"),
        "space" => KeySeq::Raw(b" "),
        "pageup" => KeySeq::Tilde(5),
        "pagedown" => KeySeq::Tilde(6),
        "insert" => KeySeq::Tilde(2),
        "delete" => KeySeq::Tilde(3),
        "f5" => KeySeq::Tilde(15),
        "f6" => KeySeq::Tilde(17),
        "f7" => KeySeq::Tilde(18),
        "f8" => KeySeq::Tilde(19),
        "f9" => KeySeq::Tilde(20),
        "f10" => KeySeq::Tilde(21),
        "f11" => KeySeq::Tilde(23),
        "f12" => KeySeq::Tilde(24),
        _ => return None,
    })
}

/// Encode a classified key into PTY bytes, honoring app-cursor mode and the
/// xterm modifier parameter.
fn encode_special(seq: KeySeq, ks: &Keystroke, mode: InputMode) -> Vec<u8> {
    match seq {
        KeySeq::Raw(bytes) => apply_alt(ks, bytes.to_vec()),
        KeySeq::Cursor(final_byte) => {
            if let Some(p) = modifier_param(ks) {
                let mut v = format!("\x1b[1;{p}").into_bytes();
                v.push(final_byte);
                v
            } else if mode.app_cursor {
                vec![0x1b, b'O', final_byte]
            } else {
                vec![0x1b, b'[', final_byte]
            }
        }
        KeySeq::Ss3(final_byte) => {
            if let Some(p) = modifier_param(ks) {
                let mut v = format!("\x1b[1;{p}").into_bytes();
                v.push(final_byte);
                v
            } else {
                vec![0x1b, b'O', final_byte]
            }
        }
        KeySeq::Tilde(n) => match modifier_param(ks) {
            Some(p) => format!("\x1b[{n};{p}~").into_bytes(),
            None => format!("\x1b[{n}~").into_bytes(),
        },
    }
}

/// xterm modifier parameter: `1 + shift + 2·alt + 4·ctrl`. `None` when no
/// modifier is held, so the caller emits the unmodified sequence. Alt is
/// folded in here for navigation keys (so Alt+Left → `ESC [ 1 ; 3 D`) rather
/// than ESC-prefixed; printable keys keep the Meta ESC-prefix via `apply_alt`.
fn modifier_param(ks: &Keystroke) -> Option<u8> {
    let m = &ks.modifiers;
    if !(m.shift || m.alt || m.control) {
        return None;
    }
    Some(1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8)
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

    fn off() -> InputMode {
        InputMode::default()
    }

    #[test]
    fn printable_chars() {
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("a"), Modifiers::default()), off()),
            b"a"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("A", Some("A"), Modifiers::default()), off()),
            b"A"
        );
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(
            keystroke_to_bytes(&ks("enter", None, Modifiers::default()), off()),
            b"\r"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("backspace", None, Modifiers::default()), off()),
            b"\x7f"
        );
    }

    #[test]
    fn tab_and_backtab() {
        // Plain Tab → HT, what the shell consumes for forward completion.
        assert_eq!(
            keystroke_to_bytes(&ks("tab", None, Modifiers::default()), off()),
            b"\t"
        );
        // Shift+Tab → CSI Z (backtab), for reverse menu completion — must not
        // collapse to a plain `\t`.
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks("tab", None, shift), off()),
            b"\x1b[Z"
        );
    }

    #[test]
    fn ime_text_key_classification() {
        let none = Modifiers::default();
        // Plain printable text + space are owned by the IME (deferred so
        // multi-keystroke composition works); uppercase via Shift too.
        assert!(is_ime_text_key(&ks("a", Some("a"), none)));
        assert!(is_ime_text_key(&ks("1", Some("1"), none)));
        assert!(is_ime_text_key(&ks("space", Some(" "), none)));
        assert!(is_ime_text_key(&ks(
            "a",
            Some("A"),
            Modifiers {
                shift: true,
                ..Default::default()
            }
        )));
        // Named navigation/control keys stay on the byte path.
        assert!(!is_ime_text_key(&ks("enter", None, none)));
        assert!(!is_ime_text_key(&ks("backspace", None, none)));
        assert!(!is_ime_text_key(&ks("escape", None, none)));
        assert!(!is_ime_text_key(&ks("left", None, none)));
        assert!(!is_ime_text_key(&ks("f1", None, none)));
        // Modified keys are shortcuts / meta combos, never IME text.
        assert!(!is_ime_text_key(&ks(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                ..Default::default()
            }
        )));
        assert!(!is_ime_text_key(&ks(
            "a",
            Some("a"),
            Modifiers {
                platform: true,
                ..Default::default()
            }
        )));
        assert!(!is_ime_text_key(&ks(
            "a",
            Some("å"),
            Modifiers {
                alt: true,
                ..Default::default()
            }
        )));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, Modifiers::default()), off()),
            b"\x1b[A"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, Modifiers::default()), off()),
            b"\x1b[C"
        );
    }

    #[test]
    fn app_cursor_mode_uses_ss3() {
        let app = InputMode { app_cursor: true };
        // DECCKM on, no modifiers → SS3 form.
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, Modifiers::default()), app),
            b"\x1bOA"
        );
        // Off → CSI form (regression guard).
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, Modifiers::default()), off()),
            b"\x1b[A"
        );
    }

    #[test]
    fn ctrl_arrow_encodes_modifier_param() {
        let m = Modifiers {
            control: true,
            ..Default::default()
        };
        // Ctrl+Right → CSI 1;5C, and the modifier form wins over app-cursor.
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, m), off()),
            b"\x1b[1;5C"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, m), InputMode { app_cursor: true }),
            b"\x1b[1;5C"
        );
    }

    #[test]
    fn shift_up_encodes_modifier_param() {
        let m = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("up", None, m), off()), b"\x1b[1;2A");
    }

    #[test]
    fn modified_tilde_key_encodes_param() {
        let m = Modifiers {
            control: true,
            ..Default::default()
        };
        // Ctrl+PageUp → ESC[5;5~; unmodified stays ESC[5~.
        assert_eq!(
            keystroke_to_bytes(&ks("pageup", None, m), off()),
            b"\x1b[5;5~"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("pageup", None, Modifiers::default()), off()),
            b"\x1b[5~"
        );
    }

    #[test]
    fn f1_uses_ss3_unmodified_and_csi_when_modified() {
        // F1-F4 are SS3 unmodified (terminfo kf1=\EOP) regardless of DECCKM.
        assert_eq!(
            keystroke_to_bytes(&ks("f1", None, Modifiers::default()), off()),
            b"\x1bOP"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f1", None, Modifiers::default()), InputMode { app_cursor: true }),
            b"\x1bOP"
        );
        // Shift+F1 → CSI 1;2P.
        let m = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("f1", None, m), off()), b"\x1b[1;2P");
    }

    #[test]
    fn ctrl_c_is_0x03() {
        let m = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("c", Some("c"), m), off()), b"\x03");
    }

    #[test]
    fn alt_a_prefixes_esc() {
        let m = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks("a", Some("a"), m), off()), b"\x1ba");
    }

    #[test]
    fn alt_with_composed_key_char_uses_unshifted_key_label() {
        // macOS Option+A delivers `key = "a"`, `key_char = "å"` (composed).
        // Meta semantics: shells expect ESC + the ASCII byte of the key, NOT
        // ESC + composed UTF-8. So bytes must be `\x1b a`, not `\x1b å`.
        let m = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("å"), m), off()),
            b"\x1ba",
            "Alt + composed key_char should still send ESC + unshifted ASCII"
        );
    }

    #[test]
    fn cmd_combos_swallowed() {
        let m = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks("c", Some("c"), m), off()),
            Vec::<u8>::new()
        );
    }
}
