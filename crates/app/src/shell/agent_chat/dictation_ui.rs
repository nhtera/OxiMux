//! Composer-side dictation UI state + helpers.
//!
//! The heavy lifting (capture, decode, model management) lives in
//! `oximux-dictation` and the [`super::dictation_service`]; this module holds
//! only the small UI state machine the composer renders and the pure text
//! helpers (smart spacing, mm:ss) that are worth unit-testing on their own.

use std::collections::VecDeque;
use std::time::Instant;

/// How many bars the scrolling recording waveform shows. Older samples fall off
/// the left as new RMS levels arrive (~10 Hz), so this is ~6 s of history. A
/// higher count gives a finer, denser comb that reads "full" (ChatGPT-style)
/// once the bars spread edge-to-edge across the recording bar.
pub const WAVEFORM_BARS: usize = 64;

/// A fixed-width ring of recent RMS levels driving the recording waveform. RMS
/// values are tiny for speech, so [`Self::bars`] amplifies + clamps them into a
/// 0..1 height with a small visible floor. Pure + unit-tested; the gpui bar
/// rendering lives in `dictation_waveform.rs` so both the composer bar and the
/// global HUD pill share one look.
#[derive(Debug, Clone, Default)]
pub struct WaveformBuffer {
    samples: VecDeque<f32>,
}

impl WaveformBuffer {
    /// Append the latest RMS level, evicting the oldest once full.
    pub fn push(&mut self, level: f32) {
        if self.samples.len() == WAVEFORM_BARS {
            self.samples.pop_front();
        }
        self.samples.push_back(level.max(0.0));
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Bar heights in `[floor, 1.0]`, oldest first. `gain` amplifies the small
    /// speech RMS; `floor` keeps quiet bars faintly visible.
    pub fn bars(&self, gain: f32, floor: f32) -> Vec<f32> {
        self.samples
            .iter()
            .map(|s| (s * gain).clamp(floor, 1.0))
            .collect()
    }

    /// Always exactly [`WAVEFORM_BARS`] heights, oldest first, LEFT-PADDED with
    /// `floor` when the ring isn't full yet. The recording bar renders this so
    /// the waveform is a full-width strip from the first frame — quiet
    /// baseline dots on the left that come alive on the right as audio arrives,
    /// instead of a short cluster that only fills once ~6 s have elapsed.
    pub fn filled_bars(&self, gain: f32, floor: f32) -> Vec<f32> {
        let live = self.bars(gain, floor);
        if live.len() >= WAVEFORM_BARS {
            return live;
        }
        let mut out = vec![floor; WAVEFORM_BARS - live.len()];
        out.extend(live);
        out
    }
}

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
    /// Actively recording. The scrolling waveform reads its levels from a
    /// separate [`WaveformBuffer`] owned by the composer/HUD, not this state.
    Recording { started_at: Instant },
    /// Recording stopped; decoding.
    Transcribing,
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

    #[test]
    fn waveform_caps_at_capacity_and_keeps_newest() {
        let mut wf = WaveformBuffer::default();
        // Small in-range values so `bars` (which clamps to [0,1]) reads them back.
        for i in 0..(WAVEFORM_BARS + 10) {
            wf.push(i as f32 * 0.001);
        }
        let bars = wf.bars(1.0, 0.0);
        assert_eq!(bars.len(), WAVEFORM_BARS, "ring capped");
        // Oldest first: the first retained sample is #10 (0..9 evicted).
        assert!((bars.first().copied().unwrap() - 0.010).abs() < 1e-6);
        assert!((bars.last().copied().unwrap() - (WAVEFORM_BARS + 9) as f32 * 0.001).abs() < 1e-6);
    }

    #[test]
    fn waveform_bars_apply_gain_floor_and_clamp() {
        let mut wf = WaveformBuffer::default();
        wf.push(0.0); // → floor
        wf.push(0.02); // 0.02 * 20 = 0.4
        wf.push(0.5); // 0.5 * 20 = 10 → clamped to 1.0
        let bars = wf.bars(20.0, 0.05);
        assert_eq!(bars[0], 0.05, "silent bar shows the floor");
        assert!((bars[1] - 0.4).abs() < 1e-6);
        assert_eq!(bars[2], 1.0, "loud bar clamps to 1.0");
    }

    #[test]
    fn filled_bars_left_pads_to_full_width() {
        let mut wf = WaveformBuffer::default();
        wf.push(0.5); // one live sample
        let bars = wf.filled_bars(1.0, 0.05);
        assert_eq!(bars.len(), WAVEFORM_BARS, "always a full-width strip");
        assert_eq!(bars[0], 0.05, "front padded with the floor");
        assert_eq!(
            *bars.last().unwrap(),
            0.5,
            "newest live sample stays at the right edge"
        );
    }

    #[test]
    fn filled_bars_matches_bars_once_full() {
        let mut wf = WaveformBuffer::default();
        for _ in 0..(WAVEFORM_BARS + 5) {
            wf.push(0.3);
        }
        assert_eq!(wf.filled_bars(1.0, 0.05), wf.bars(1.0, 0.05));
    }

    #[test]
    fn waveform_clear_empties() {
        let mut wf = WaveformBuffer::default();
        wf.push(0.3);
        assert!(!wf.bars(1.0, 0.0).is_empty());
        wf.clear();
        assert!(wf.bars(1.0, 0.0).is_empty());
    }
}
