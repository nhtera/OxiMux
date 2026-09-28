//! The document the platform input method sees in a terminal.
//!
//! A terminal has no text buffer an input method could read, but Vietnamese
//! Telex needs one: it rewrites letters it already committed. After the caret
//! moves (a click), it commits a word's first letter outright and, on the next
//! key, reads it back and replaces it (`o`, then `o` → `ô`). A browser text
//! field holds those letters, so Orca composes; with nothing to read back the
//! terminal typed "oo". So the terminal keeps what the input method committed
//! since the caret last moved another way, followed by the marked text, and
//! erases committed letters in the shell (Backspace) when the input method
//! replaces them. Offsets are UTF-16, as the platform counts them.

use std::ops::Range;

/// Longest committed tail kept: more than one word, which is all Telex reads.
const KEEP_CHARS: usize = 64;

/// What the input method committed since the caret last moved another way.
#[derive(Default)]
pub(super) struct ImeTyped(String);

impl ImeTyped {
    fn len16(&self) -> usize {
        utf16_len(&self.0)
    }

    /// Where the marked text starts in the document (the shell's caret).
    pub(super) fn marked_start(&self) -> usize {
        self.len16()
    }

    /// The caret: after the committed text and any marked text.
    pub(super) fn caret(&self, marked: Option<&str>) -> usize {
        self.len16() + marked.map_or(0, utf16_len)
    }

    /// The document's text in `range` (clamped, widened so it never splits a
    /// surrogate pair), with the range it covers.
    pub(super) fn text(&self, marked: Option<&str>, range: Range<usize>) -> (String, Range<usize>) {
        let doc: Vec<u16> = self.0.encode_utf16().chain(marked.unwrap_or_default().encode_utf16()).collect();
        let low_surrogate = |i: usize| doc.get(i).is_some_and(|u| (0xDC00..0xE000).contains(u));
        let mut start = range.start.min(doc.len());
        let mut end = range.end.clamp(start, doc.len());
        if low_surrogate(start) {
            start -= 1;
        }
        if low_surrogate(end) {
            end += 1;
        }
        (String::from_utf16_lossy(&doc[start..end]), start..end)
    }

    /// The input method replaces `range`: take back the committed letters it
    /// covers. Returns how many characters the shell must erase. Only a range
    /// that reaches the caret is honored — Backspace can't erase a middle
    /// slice and leave the letters after it, so anything else inserts at the
    /// caret rather than erase letters the input method did not ask to touch.
    pub(super) fn rewind(&mut self, range: Option<&Range<usize>>) -> usize {
        let len = self.len16();
        let Some(range) = range.filter(|r| r.start < len && r.end >= len) else { return 0 };
        let at = byte_at_utf16(&self.0, range.start);
        let erased = self.0[at..].chars().count();
        self.0.truncate(at);
        erased
    }

    /// The input method committed `text` at the caret.
    pub(super) fn committed(&mut self, text: &str) {
        self.0.push_str(text);
        let extra = self.0.chars().count().saturating_sub(KEEP_CHARS);
        if extra > 0 {
            let at = self.0.char_indices().nth(extra).map_or(self.0.len(), |(i, _)| i);
            self.0.drain(..at);
        }
    }

    /// A Backspace reached the shell: the last committed letter is gone.
    pub(super) fn backspace(&mut self) {
        self.0.pop();
    }

    /// The caret moved another way (a click, a key the input method did not
    /// take, a paste, focus): what it committed is no longer before the caret.
    pub(super) fn reset(&mut self) {
        self.0.clear();
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn byte_at_utf16(s: &str, offset: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= offset {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Telex after a click: `o` is committed, the next `o` reads it back and
    /// rewrites it as marked "ô", so the shell erases the one letter it has.
    #[test]
    fn a_rewritten_letter_is_erased_and_the_document_follows() {
        let mut typed = ImeTyped::default();
        typed.committed("o");
        assert_eq!(typed.caret(None), 1);
        assert_eq!(typed.text(None, 0..200), ("o".into(), 0..1));
        assert_eq!(typed.rewind(Some(&(0..1))), 1, "the shell erases the o");
        assert_eq!(typed.marked_start(), 0);
        let marked = Some("ông");
        assert_eq!(typed.caret(marked), 3);
        assert_eq!(typed.text(marked, 0..3).0, "ông");
        typed.committed("ông");
        assert_eq!(typed.rewind(None), 0, "a plain commit erases nothing");
        assert_eq!(typed.rewind(Some(&(3..3))), 0, "nor one at the caret");
    }

    /// Offsets are UTF-16; erasing counts characters (what Backspace removes).
    #[test]
    fn offsets_are_utf16_and_erasing_counts_characters() {
        let mut typed = ImeTyped::default();
        typed.committed("bà 🙂x");
        assert_eq!(typed.caret(None), 6, "the emoji is two UTF-16 units");
        assert_eq!(typed.text(None, 4..5), ("🙂".into(), 3..5), "never half a pair");
        assert_eq!(typed.rewind(Some(&(3..6))), 2, "🙂 and x");
        assert_eq!(typed.text(None, 0..10).0, "bà ");
    }

    /// A range short of the caret would need the letters after it retyped;
    /// it erases nothing instead.
    #[test]
    fn a_middle_range_erases_nothing() {
        let mut typed = ImeTyped::default();
        typed.committed("abc");
        assert_eq!(typed.rewind(Some(&(0..1))), 0);
        assert_eq!(typed.text(None, 0..10).0, "abc");
    }

    #[test]
    fn backspace_reset_and_the_cap() {
        let mut typed = ImeTyped::default();
        typed.committed("việt");
        typed.backspace();
        assert_eq!(typed.text(None, 0..10).0, "việ");
        typed.reset();
        assert_eq!(typed.caret(None), 0);
        typed.committed(&"a".repeat(KEEP_CHARS + 10));
        assert_eq!(typed.caret(None), KEEP_CHARS);
    }
}
