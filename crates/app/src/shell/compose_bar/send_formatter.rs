//! Pure formatter that turns a composed draft into the exact bytes written to
//! an agent's PTY.
//!
//! When the agent's shell has DECSET-2004 (bracketed paste) enabled, the draft
//! is wrapped in `ESC[200~ … ESC[201~` so a multi-line prompt is inserted as a
//! single chunk — readline/zle treat it as one paste rather than executing each
//! line. A trailing carriage return is then appended *outside* the paste
//! brackets so a single ⌘↵ both delivers AND submits the draft (matching the
//! "send" affordance). When bracketed paste is off, the raw draft bytes plus
//! the CR are sent verbatim.
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
/// - `bp_on` true → wrap in bracketed-paste brackets, then a trailing CR.
/// - `bp_on` false → raw draft bytes, then a trailing CR.
///
/// The ESC byte is removed from the draft payload in both cases. The trailing
/// CR sits OUTSIDE the paste brackets so the agent's readline inserts the block
/// and then executes it — one ⌘↵ delivers and submits.
pub fn format_send_bytes(draft: &str, bp_on: bool) -> Vec<u8> {
    let payload: Vec<u8> = draft.bytes().filter(|b| *b != 0x1b).collect();
    let mut out = if bp_on {
        let mut wrapped =
            Vec::with_capacity(payload.len() + PASTE_START.len() + PASTE_END.len() + 1);
        wrapped.extend_from_slice(PASTE_START);
        wrapped.extend_from_slice(&payload);
        wrapped.extend_from_slice(PASTE_END);
        wrapped
    } else {
        payload
    };
    out.push(b'\r');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_formatter_bracketed_paste_submits_with_trailing_cr() {
        let out = format_send_bytes("hello", true);
        assert!(out.starts_with(PASTE_START), "wrapped with paste-start");
        // CR is appended OUTSIDE the closing bracket, so the payload submits.
        assert_eq!(out.last(), Some(&b'\r'), "trailing CR submits the draft");
        assert!(
            out[..out.len() - 1].ends_with(PASTE_END),
            "paste-end precedes the CR",
        );
    }

    #[test]
    fn send_formatter_plain_appends_cr() {
        // Raw bytes plus the submit CR when bracketed paste is off.
        assert_eq!(format_send_bytes("hello", false), b"hello\r");
    }

    #[test]
    fn send_formatter_strips_esc_from_payload() {
        let out = format_send_bytes("hello\u{1b}world", true);
        // Payload sits between the brackets; the trailing byte is the CR.
        let start = PASTE_START.len();
        let end = out.len() - PASTE_END.len() - 1;
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
        let inner = &out[PASTE_START.len()..out.len() - PASTE_END.len() - 1];
        assert_eq!(inner, b"line1\nline2");
    }
}
