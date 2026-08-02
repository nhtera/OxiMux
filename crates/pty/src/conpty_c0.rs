//! Reconcile ConPTY's column accounting with VT's for the C0 bytes the two
//! disagree about.
//!
//! # The disagreement
//!
//! ConPTY renders its child's output into a Windows console screen buffer and
//! then re-encodes that buffer as a VT stream. The two models do not agree on
//! what a bare `0x0E`/`0x0F` byte is worth:
//!
//! - The **console buffer** inherits DOS semantics, where most C0 bytes are
//!   printable CP437 glyphs. `0x0F` is `☼`. It occupies **one column**.
//! - A **VT terminal** reads `0x0E`/`0x0F` as SO/SI — shift-out / shift-in,
//!   controls that select a character set and occupy **zero columns**.
//!
//! ConPTY forwards the byte verbatim rather than re-encoding it as the glyph
//! its own buffer holds. So the moment a child emits one, ConPTY's idea of the
//! cursor column is one ahead of the terminal's, and it stays ahead for the
//! rest of the line. Every *relative* cursor op ConPTY emits after that —
//! notably the backspace it uses to move left by one — lands a column too far
//! left, and the text it then writes overwrites a cell it should have skipped.
//!
//! Measured on Windows 10 19045, via a child writing a lone `0x0F` through a
//! ConPTY: `[Console]::CursorLeft` reports **1**, and the byte arrives at the
//! terminal **unchanged**. Both halves of the mismatch in one observation.
//!
//! Not every ConPTY drifts: the windows-2025 CI image gives the byte zero
//! width in its own buffer and never forwards it, so there is nothing to
//! reconcile and this filter idles. The snapshot smoke test therefore asserts
//! column *parity* against `[Console]::CursorLeft` rather than a literal
//! frame, which also guards the reverse failure — this filter spacing a byte
//! a fixed ConPTY gave no width to.
//!
//! # What this cost in practice
//!
//! Claude Code emits a stray `0x0F` on the first redraw of its input line. Its
//! prompt is `U+276F` + `U+00A0`, so text belongs in column 2; after the drift
//! the typed character landed in column 1, overwriting the separator, and each
//! later keystroke printed at the drifted cursor:
//!
//! ```text
//!   want:  ❯ hi
//!   got:   ❯h i
//! ```
//!
//! It healed as soon as something forced a full repaint — every cell addressed
//! absolutely — which is why sending the first message appeared to "fix" it.
//!
//! # The reconciliation
//!
//! Substitute a space for those bytes, so the cell they occupy in ConPTY's
//! buffer is a cell in ours too. A space rather than the CP437 glyph: the
//! column is what the cursor arithmetic depends on, and the glyph is noise
//! from an emitter that did not mean to send a control byte at all. Here the
//! substituted cell is overwritten within the same frame and never shown.
//!
//! Ground state only. Inside an escape, CSI, OSC or DCS sequence these bytes
//! are the emitter's business and are passed through untouched — a `0x0F` in
//! an OSC title payload is not a column.
//!
//! Windows only ([`ConptyC0Filter::apply`]). Elsewhere SO/SI mean what the VT
//! spec says and a program using them deserves to be obeyed. Applying this at
//! the PTY read boundary (rather than in the grid) is deliberate: the
//! corrected stream is what reaches the local grid *and* the relay wire, so a
//! remote client attached to a Windows daemon is fixed by the same edit.

use std::borrow::Cow;

/// Bytes ConPTY gives a column to and a VT terminal does not.
const SHIFT_OUT: u8 = 0x0E;
const SHIFT_IN: u8 = 0x0F;

/// Enough of a VT state machine to answer one question: are we in ground
/// state, where a byte is screen content, or inside a sequence, where it is
/// the emitter's payload?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ground,
    /// Saw ESC; the next byte says what kind of sequence this is.
    Esc,
    /// Inside a CSI sequence, up to and including its final byte.
    Csi,
    /// Inside a string sequence (OSC/DCS/SOS/PM/APC), up to BEL or ST.
    Str,
}

/// Per-session, because a sequence can span read chunks.
#[derive(Debug)]
pub(crate) struct ConptyC0Filter {
    phase: Phase,
}

impl ConptyC0Filter {
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Ground,
        }
    }

    /// Reconcile `bytes` on Windows; hand them back untouched everywhere else.
    ///
    /// Borrows when there is nothing to change, which is every chunk that
    /// carries no bare SO/SI — i.e. essentially all of them.
    pub(crate) fn apply<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if cfg!(windows) {
            self.translate(bytes)
        } else {
            Cow::Borrowed(bytes)
        }
    }

    /// The platform-independent core, so the state machine is exercised by
    /// the test suite on every host rather than only where it takes effect.
    fn translate<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        // Advance the phase over the whole chunk first and note the offsets
        // worth rewriting. Nothing to rewrite is the overwhelmingly common
        // case, and it costs no allocation.
        let mut hits: Vec<usize> = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            match self.phase {
                Phase::Ground => match b {
                    0x1B => self.phase = Phase::Esc,
                    SHIFT_OUT | SHIFT_IN => hits.push(i),
                    _ => {}
                },
                Phase::Esc => {
                    self.phase = match b {
                        b'[' => Phase::Csi,
                        // OSC, DCS, SOS, PM, APC — all run to BEL or ST.
                        b']' | b'P' | b'X' | b'^' | b'_' => Phase::Str,
                        // A second ESC restarts the sequence rather than
                        // being consumed as its type byte.
                        0x1B => Phase::Esc,
                        _ => Phase::Ground,
                    }
                }
                Phase::Csi => match b {
                    // Final byte ends the sequence.
                    0x40..=0x7E => self.phase = Phase::Ground,
                    0x1B => self.phase = Phase::Esc,
                    _ => {}
                },
                Phase::Str => match b {
                    0x07 => self.phase = Phase::Ground,
                    // ST is ESC \, and the Esc arm above sends any other
                    // follower back to Ground, which is the right recovery
                    // for a malformed string terminator anyway.
                    0x1B => self.phase = Phase::Esc,
                    _ => {}
                },
            }
        }
        if hits.is_empty() {
            return Cow::Borrowed(bytes);
        }
        let mut owned = bytes.to_vec();
        for i in hits {
            owned[i] = b' ';
        }
        Cow::Owned(owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(f: &mut ConptyC0Filter, bytes: &[u8]) -> Vec<u8> {
        f.translate(bytes).into_owned()
    }

    #[test]
    fn a_bare_shift_in_becomes_a_space() {
        let mut f = ConptyC0Filter::new();
        assert_eq!(translate(&mut f, b"a\x0fb"), b"a b".to_vec());
    }

    #[test]
    fn a_bare_shift_out_becomes_a_space() {
        let mut f = ConptyC0Filter::new();
        assert_eq!(translate(&mut f, b"a\x0eb"), b"a b".to_vec());
    }

    #[test]
    fn ordinary_output_is_borrowed_not_copied() {
        // The no-op path is the one that runs on every chunk of every
        // session; it must not allocate.
        let mut f = ConptyC0Filter::new();
        assert!(matches!(
            f.translate(b"\x1b[31mhello\x1b[m\r\n"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn the_captured_regression_lands_the_glyph_in_the_right_column() {
        // The exact shape ConPTY emitted for Claude Code's first keystroke.
        // With the 0x0F given a column, the backspace moves from column 3
        // to column 2 and `h` lands in column 2 (1-based) — over the
        // substituted space, not over the prompt separator.
        let mut f = ConptyC0Filter::new();
        let got = translate(&mut f, b"\x0f\x1b[?2026h\x1b[?2026l\x08h\x1b[K");
        assert_eq!(got, b" \x1b[?2026h\x1b[?2026l\x08h\x1b[K".to_vec());
    }

    #[test]
    fn a_shift_in_inside_an_osc_payload_is_left_alone() {
        // OSC 0 sets a window title; a 0x0F in there is the emitter's data
        // and occupies no column on anyone's screen.
        let mut f = ConptyC0Filter::new();
        let seq = b"\x1b]0;ti\x0ftle\x07after";
        assert_eq!(translate(&mut f, seq), seq.to_vec());
    }

    #[test]
    fn a_shift_in_inside_a_csi_sequence_is_left_alone() {
        let mut f = ConptyC0Filter::new();
        let seq = b"\x1b[3\x0f1m";
        assert_eq!(translate(&mut f, seq), seq.to_vec());
    }

    #[test]
    fn a_string_terminated_by_st_returns_to_ground() {
        // ESC \ ends the OSC; the 0x0F after it is screen content again.
        let mut f = ConptyC0Filter::new();
        let got = translate(&mut f, b"\x1b]0;t\x1b\\\x0fx");
        assert_eq!(got, b"\x1b]0;t\x1b\\ x".to_vec());
    }

    #[test]
    fn phase_survives_a_chunk_boundary_mid_osc() {
        // The read loop splits wherever the pipe does. A sequence cut in
        // half must not let its payload be rewritten as ground-state text.
        let mut f = ConptyC0Filter::new();
        assert_eq!(translate(&mut f, b"\x1b]0;ti"), b"\x1b]0;ti".to_vec());
        assert_eq!(translate(&mut f, b"\x0ftle\x07"), b"\x0ftle\x07".to_vec());
        // ...and ground state resumes on the far side.
        assert_eq!(translate(&mut f, b"\x0f"), b" ".to_vec());
    }

    #[test]
    fn phase_survives_a_chunk_boundary_mid_csi() {
        let mut f = ConptyC0Filter::new();
        assert_eq!(translate(&mut f, b"\x1b["), b"\x1b[".to_vec());
        assert_eq!(translate(&mut f, b"\x0f1m\x0f"), b"\x0f1m ".to_vec());
    }

    #[test]
    fn an_escape_split_from_its_type_byte_still_opens_the_sequence() {
        let mut f = ConptyC0Filter::new();
        assert_eq!(translate(&mut f, b"\x1b"), b"\x1b".to_vec());
        assert_eq!(translate(&mut f, b"]0;\x0f\x07"), b"]0;\x0f\x07".to_vec());
    }
}
