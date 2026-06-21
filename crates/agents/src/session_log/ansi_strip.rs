//! Strip ANSI / OSC control sequences from raw terminal bytes.
//!
//! Scrollback captured from a PTY is dense with SGR color runs, cursor
//! moves, and OSC title/hyperlink sequences. When that scrollback is reused
//! as context for a forked agent session it has to become plain, readable
//! text. This is a single-pass state machine over `char`s: every
//! ESC-initiated sequence is dropped, all printable text and whitespace pass
//! through unchanged.
//!
//! PTY output is untrusted, so the machine never panics — unrecognized or
//! truncated sequences fall through, and any dangling ESC at end-of-input is
//! simply discarded. It deliberately does NOT scrub lone C0 control bytes
//! (a stray BEL, CR, etc.): the single responsibility here is escape-sequence
//! removal, and callers that want whitespace flattening compose their own.

const ESC: char = '\u{1b}';
const BEL: char = '\u{07}';

/// Remove ANSI escape sequences — CSI/SGR, OSC, charset designators, the
/// short two-byte C1 escapes, and DCS/SOS/PM/APC string sequences — from
/// `input`, preserving all printable text and whitespace.
pub fn strip_ansi_osc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut state = State::Text;
    for c in input.chars() {
        state = step(state, c, &mut out);
    }
    out
}

#[derive(Clone, Copy)]
enum State {
    /// Outside any escape — printable bytes copy through here.
    Text,
    /// Saw ESC; the next byte selects the sequence kind.
    Esc,
    /// Inside `ESC [ …` (CSI) — consume to a final byte `0x40..=0x7E`.
    Csi,
    /// Saw `ESC` + an intermediate `0x20..=0x2F` (e.g. charset designator
    /// `ESC ( B`) — consume to a final byte `0x30..=0x7E`.
    EscIntermediate,
    /// Inside an OSC/DCS/SOS/PM/APC string — consume to BEL or ST.
    Str,
    /// Inside a string and just saw ESC — a following `\` is the ST
    /// terminator; anything else also ends the string (and is dropped).
    StrEsc,
}

fn step(state: State, c: char, out: &mut String) -> State {
    match state {
        State::Text => {
            if c == ESC {
                State::Esc
            } else {
                out.push(c);
                State::Text
            }
        }
        State::Esc => match c {
            '[' => State::Csi,
            // OSC (']'), DCS ('P'), SOS ('X'), PM ('^'), APC ('_') all
            // introduce a string terminated by BEL or ST.
            ']' | 'P' | 'X' | '^' | '_' => State::Str,
            // Intermediate byte → multi-byte escape like `ESC ( B`.
            '\u{20}'..='\u{2f}' => State::EscIntermediate,
            // Two-byte C1 escape (ESC =, ESC M, …) — drop both bytes.
            _ => State::Text,
        },
        // CSI: params 0x30-0x3F + intermediates 0x20-0x2F precede a final
        // byte in 0x40-0x7E that ends the sequence.
        State::Csi => {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                State::Text
            } else {
                State::Csi
            }
        }
        State::EscIntermediate => {
            if ('\u{30}'..='\u{7e}').contains(&c) {
                State::Text
            } else {
                State::EscIntermediate
            }
        }
        State::Str => match c {
            BEL => State::Text,
            ESC => State::StrEsc,
            _ => State::Str,
        },
        // ESC inside a string: `\` completes ST; any other byte is malformed
        // but still terminates (and is consumed) rather than hanging.
        State::StrEsc => State::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_strip_removes_sgr_and_osc() {
        // The two locked success-criteria equalities.
        assert_eq!(strip_ansi_osc("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_osc("\x1b]7;file:///tmp\x07text"), "text");
    }

    #[test]
    fn osc_string_terminated_by_st_is_removed() {
        // OSC-8 hyperlink, terminated by ST (ESC \) instead of BEL.
        let input = "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(strip_ansi_osc(input), "link");
    }

    #[test]
    fn charset_designator_is_fully_removed() {
        // `ESC ( B` selects the ASCII charset — three bytes, no stray 'B'.
        assert_eq!(strip_ansi_osc("\x1b(Bplain"), "plain");
    }

    #[test]
    fn two_byte_c1_escape_is_removed() {
        // ESC M (Reverse Index) — both bytes dropped, no stray 'M'.
        assert_eq!(strip_ansi_osc("a\x1bMb"), "ab");
    }

    #[test]
    fn preserves_printable_unicode_and_whitespace() {
        let s = "héllo\tworld\n→ ok";
        assert_eq!(strip_ansi_osc(s), s);
    }

    #[test]
    fn does_not_panic_on_malformed_or_truncated() {
        // Lone ESC, truncated CSI, truncated OSC — none may panic, and the
        // dangling escape state is dropped at end-of-input.
        assert_eq!(strip_ansi_osc("text\x1b"), "text");
        assert_eq!(strip_ansi_osc("text\x1b[31"), "text");
        assert_eq!(strip_ansi_osc("text\x1b]7;unterminated"), "text");
        assert_eq!(strip_ansi_osc(""), "");
    }

    #[test]
    fn multiple_sequences_interleaved_with_text() {
        let input = "\x1b[1;32m$ \x1b[0mcargo \x1b[33mtest\x1b[0m\n\x1b]0;title\x07done";
        assert_eq!(strip_ansi_osc(input), "$ cargo test\ndone");
    }
}
