//! Composer-side dictation UI state + helpers.
//!
//! The heavy lifting (capture, decode, model management) lives in
//! `oximux-dictation` and the [`super::dictation_service`]; this module holds
//! only the small UI state machine the composer renders and the pure text
//! helpers (smart spacing, mm:ss) that are worth unit-testing on their own.

use std::time::Instant;

/// What the composer is doing with dictation right now. Drives the mic button
/// glyph and the recording pill.
#[derive(Debug, Clone, Default)]
pub enum DictationUiState {
    /// Not recording — the plain mic button.
    #[default]
    Idle,
    /// Permission/model check in flight, or the engine is still warming while
    /// capture buffers.
    Starting,
    /// Actively recording. `level` is the latest RMS (0..~1) for the meter.
    Recording { started_at: Instant, level: f32 },
    /// Recording stopped; decoding.
    Transcribing,
    /// Last attempt failed; the message is surfaced as a toast and the state
    /// falls back to Idle on the next action.
    Failed(String),
}

impl DictationUiState {
    /// True while a session is live in any pre-transcript phase (button shows a
    /// stop affordance).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            DictationUiState::Starting
                | DictationUiState::Recording { .. }
                | DictationUiState::Transcribing
        )
    }
}

/// Punctuation that should hug the preceding word (no leading space inserted
/// before it).
fn starts_with_close_punct(s: &str) -> bool {
    matches!(
        s.chars().next(),
        Some(',' | '.' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '%' | '…')
    )
}

/// Build the string to insert at the cursor, adding a single leading space only
/// when it reads naturally: the text before the cursor is non-empty, does not
/// already end in whitespace, and the transcript does not begin with closing
/// punctuation. Mirrors the reference apps' smart-space rule (the CJK no-space
/// case doesn't apply to Vietnamese, but the punctuation rule does).
pub fn dictation_spacing(before_cursor: &str, transcript: &str) -> String {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return String::new();
    }
    let needs_space = !before_cursor.is_empty()
        && !before_cursor.ends_with(char::is_whitespace)
        && !starts_with_close_punct(transcript);
    if needs_space {
        format!(" {transcript}")
    } else {
        transcript.to_string()
    }
}

/// Elapsed recording time as `m:ss` (no hours — the hard cap is 2 minutes).
pub fn format_elapsed(started_at: Instant) -> String {
    let secs = started_at.elapsed().as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_transcript_inserts_nothing() {
        assert_eq!(dictation_spacing("hello", "   "), "");
        assert_eq!(dictation_spacing("", ""), "");
    }

    #[test]
    fn empty_input_no_leading_space() {
        assert_eq!(dictation_spacing("", "xin chào"), "xin chào");
    }

    #[test]
    fn trailing_space_no_double_space() {
        assert_eq!(dictation_spacing("hello ", "world"), "world");
    }

    #[test]
    fn mid_sentence_gets_a_space() {
        assert_eq!(dictation_spacing("hello", "world"), " world");
    }

    #[test]
    fn punctuation_hugs_preceding_word() {
        assert_eq!(dictation_spacing("hello", ", world"), ", world");
        assert_eq!(dictation_spacing("done", "."), ".");
    }

    #[test]
    fn vietnamese_diacritics_preserved() {
        // The helper must not mangle multibyte chars — a mid-sentence insert of
        // Vietnamese text just gets a leading space.
        assert_eq!(dictation_spacing("Tôi", "nói tiếng Việt"), " nói tiếng Việt");
    }

    #[test]
    fn transcript_is_trimmed() {
        assert_eq!(dictation_spacing("", "  hi there  "), "hi there");
    }
}
