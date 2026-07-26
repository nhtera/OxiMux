//! Pure left-truncation logic for a single match line.
//!
//! Keeps the matched span visible inside a narrow row by chopping at most
//! `BEFORE_MAX` bytes of pre-match text and prepending an ellipsis when
//! content was dropped.
//!
//! All offsets are **byte** offsets so the function survives multi-byte chars
//! (CJK, emoji). `str::is_char_boundary` guards every slice.

/// Maximum number of pre-match bytes to retain before the matched span.
pub const BEFORE_MAX: usize = 26;

/// Result of left-truncating one line around the matched span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedLine {
    /// Pre-match content; may be prefixed by `…` when bytes were dropped.
    pub before: String,
    pub matched: String,
    pub after: String,
}

/// Truncate `line_content` so the matched span at `[col_byte, col_byte+match_len)`
/// stays visible. Trailing `\n` (and `\r`) on the after segment is stripped so
/// the row text doesn't render a hard break.
///
/// Out-of-bounds inputs (col past end, match longer than line) degrade to an
/// empty `matched` rather than panicking — matches ripgrep's "binary span"
/// tolerance.
pub fn truncate_before(line_content: &str, col_byte: usize, match_len: usize) -> TruncatedLine {
    let bytes = line_content.as_bytes();
    let len = bytes.len();

    let start = col_byte.min(len);
    let end = start.saturating_add(match_len).min(len);

    // Snap match boundaries to UTF-8 char boundaries; if the caller handed us
    // mid-codepoint offsets, expand to the nearest left/right boundary.
    let start = snap_left(line_content, start);
    let end = snap_right(line_content, end);

    let raw_before = &line_content[..start];
    let matched = line_content[start..end].to_string();
    let raw_after = strip_trailing_newline(&line_content[end..]);

    let before = if raw_before.len() <= BEFORE_MAX {
        raw_before.to_string()
    } else {
        // Snap right so we don't slice mid-codepoint.
        let cut = snap_right(raw_before, raw_before.len() - BEFORE_MAX);
        format!("…{}", &raw_before[cut..])
    };

    TruncatedLine {
        before,
        matched,
        after: raw_after.to_string(),
    }
}

fn snap_left(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn snap_right(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn strip_trailing_newline(s: &str) -> &str {
    s.strip_suffix('\n')
        .unwrap_or(s)
        .strip_suffix('\r')
        .unwrap_or_else(|| s.strip_suffix('\n').unwrap_or(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_before_returns_full_prefix() {
        let t = truncate_before("hello world", 6, 5);
        assert_eq!(t.before, "hello ");
        assert_eq!(t.matched, "world");
        assert_eq!(t.after, "");
    }

    #[test]
    fn before_exactly_at_cap_no_ellipsis() {
        let prefix: String = "a".repeat(BEFORE_MAX);
        let line = format!("{prefix}MATCH after");
        let t = truncate_before(&line, BEFORE_MAX, 5);
        assert_eq!(t.before, prefix);
        assert_eq!(t.matched, "MATCH");
        assert_eq!(t.after, " after");
    }

    #[test]
    fn before_over_cap_gets_ellipsis_and_tail_only() {
        let long_prefix: String = "x".repeat(BEFORE_MAX + 10);
        let line = format!("{long_prefix}HIT tail");
        let t = truncate_before(&line, long_prefix.len(), 3);
        assert_eq!(t.matched, "HIT");
        assert_eq!(t.after, " tail");
        assert!(t.before.starts_with('…'), "expected leading ellipsis");
        // Trailing tail is exactly BEFORE_MAX bytes.
        let tail = t.before.trim_start_matches('…');
        assert_eq!(tail.len(), BEFORE_MAX);
    }

    #[test]
    fn match_at_col_zero_has_empty_before() {
        let t = truncate_before("FileStatus rest", 0, 10);
        assert_eq!(t.before, "");
        assert_eq!(t.matched, "FileStatus");
        assert_eq!(t.after, " rest");
    }

    #[test]
    fn multi_byte_chars_dont_panic_and_keep_match_intact() {
        // 2-byte char per "é" — 6 bytes prefix, 5 byte match, 1 byte after.
        let line = "ééé MATCH!"; // bytes: 2*3 + 1 + 5 + 1
        let prefix_bytes = "ééé ".len();
        let t = truncate_before(line, prefix_bytes, "MATCH".len());
        assert_eq!(t.matched, "MATCH");
        assert_eq!(t.before, "ééé ");
        assert_eq!(t.after, "!");
    }

    #[test]
    fn trailing_newline_stripped_from_after() {
        let t = truncate_before("hit here\n", 0, 3);
        assert_eq!(t.matched, "hit");
        assert_eq!(t.after, " here");
    }

    #[test]
    fn out_of_bounds_col_returns_empty_match_no_panic() {
        let t = truncate_before("short line", 999, 5);
        assert_eq!(t.matched, "");
        // before is the whole line (no ellipsis because the line is short).
        assert_eq!(t.before, "short line");
        assert_eq!(t.after, "");
    }

    #[test]
    fn match_length_overshoot_clamps_to_end() {
        let t = truncate_before("abc", 1, 999);
        assert_eq!(t.matched, "bc");
        assert_eq!(t.before, "a");
    }
}
