//! Pure formatter that turns a composed draft into the exact bytes written to
//! an agent's PTY.
//!
//! When the agent's shell has DECSET-2004 (bracketed paste) enabled, the draft
//! is wrapped in `ESC[200~ … ESC[201~` so a multi-line prompt is inserted as a
//! single chunk — readline/zle treat it as one paste rather than executing each
//! line. The wrapped form intentionally omits a trailing carriage return: the
//! composer is a *draft* aid, so the text lands in the agent's input line and
//! the user presses Enter to submit (matching the existing terminal paste
//! path). When bracketed paste is off, the raw draft bytes are sent verbatim.
//!
//! In every mode the ESC byte (`0x1b`) is stripped from the draft first, so a
//! draft can never smuggle its own escape sequence (e.g. an embedded
//! `ESC[201~` that would prematurely close the bracket) into the stream. This
//! mirrors the terminal view's paste sanitizer.

/// DECSET-2004 paste-start / paste-end brackets.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Format a composed `draft` for delivery to an agent PTY.
///
/// - `bp_on` true → wrap in bracketed-paste brackets, no trailing CR.
/// - `bp_on` false → raw draft bytes, no trailing CR.
///
/// The ESC byte is removed from the draft payload in both cases.
pub fn format_send_bytes(draft: &str, bp_on: bool) -> Vec<u8> {
    let payload: Vec<u8> = draft.bytes().filter(|b| *b != 0x1b).collect();
    if !bp_on {
        return payload;
    }
    let mut out = Vec::with_capacity(payload.len() + PASTE_START.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(&payload);
    out.extend_from_slice(PASTE_END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_formatter_bracketed_paste_no_cr() {
        let out = format_send_bytes("hello", true);
        assert!(out.starts_with(PASTE_START), "wrapped with paste-start");
        assert!(out.ends_with(PASTE_END), "wrapped with paste-end");
        assert!(!out.contains(&b'\r'), "no carriage return — user submits");
    }

    #[test]
    fn send_formatter_plain_no_bracket() {
        // Raw bytes when bracketed paste is off — and still no CR.
        assert_eq!(format_send_bytes("hello", false), b"hello");
    }

    #[test]
    fn send_formatter_strips_esc_from_payload() {
        let out = format_send_bytes("hello\u{1b}world", true);
        // The ONLY ESC bytes left are the two bracket markers; the payload
        // between them must be ESC-free.
        let start = PASTE_START.len();
        let end = out.len() - PASTE_END.len();
        assert!(
            !out[start..end].contains(&0x1b),
            "payload region is ESC-free",
        );
        // And the visible text is intact minus the ESC.
        assert_eq!(&out[start..end], b"helloworld");
    }

    #[test]
    fn send_formatter_multiline_draft_wrapped_as_one_chunk() {
        let out = format_send_bytes("line1\nline2", true);
        // Newlines are preserved inside the bracket (bracketed paste is exactly
        // the mechanism that stops them from executing line-by-line).
        let inner = &out[PASTE_START.len()..out.len() - PASTE_END.len()];
        assert_eq!(inner, b"line1\nline2");
    }
}
