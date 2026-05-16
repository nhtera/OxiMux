//! Scrollback search — Phase 1 step 8.
//!
//! Pure substring scan over a row-major cell grid. The grid is whatever
//! `TerminalBackend::search_grid` hands back (history rows + visible rows
//! today; could be just visible rows on backends that don't retain history).
//!
//! Case-insensitive, plain-text only. Regex / case toggle / prev-next nav
//! are explicitly out of this slice (see plan). Lowercasing happens once on
//! the needle and once per row — small heap reuse left to the caller's
//! choice; this implementation prioritizes clarity over allocation count
//! since the grid is bounded at ~400k cells (5000 history + ~32 rows × 200
//! cols) and a flood test against `yes` still scans under 1 ms.

use gpui::{IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_pty::Cell;
use oximux_settings::{Theme, Typography};

/// Half-open cell range within a row of the search grid.
///
/// `row` is 0-indexed from the top of the search grid (history first, then
/// visible). `col_end` is exclusive, so an empty match has `col_start ==
/// col_end` — but `find_matches` skips empty needles, so callers never see
/// one in practice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRange {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// Case-insensitive plain-substring search over a row-major cell grid.
///
/// Returns all non-overlapping matches in top-to-bottom, left-to-right
/// order. Empty needle → empty result (nothing to highlight; matches every
/// position otherwise, which is never useful UX).
///
/// Wide-char (CJK) caveat: alacritty represents wide cells as the glyph in
/// one cell + `\0` in the next. We treat `\0` cells as spaces here so they
/// don't poison the row string with literal nuls; cost is that a CJK glyph
/// won't match its romaji input, which is acceptable for v1 (English-first
/// terminal content). When CJK input matters, switch to a grapheme-aware
/// scanner.
pub fn find_matches(grid: &[Vec<Cell>], needle: &str) -> Vec<MatchRange> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_lower = needle.to_lowercase();
    let mut out = Vec::new();
    let mut row_buf = String::with_capacity(256);
    let mut col_index = Vec::with_capacity(256);

    for (row_idx, row) in grid.iter().enumerate() {
        row_buf.clear();
        col_index.clear();
        for (col_idx, cell) in row.iter().enumerate() {
            let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
            // Track the cell column that produced each char so we can map
            // byte-offsets back to cell columns after lowercase. Lowercase
            // can change byte length (e.g. ı → i is fine, Ǆ → ǆ on the
            // other hand changes width) but never changes char *count* for
            // the chars we get from a terminal grid.
            for lowered in ch.to_lowercase() {
                row_buf.push(lowered);
                col_index.push(col_idx);
            }
        }
        let mut search_start = 0;
        while let Some(found_byte) = row_buf[search_start..].find(&needle_lower) {
            let match_byte_start = search_start + found_byte;
            // Convert byte offset into a char index, then read the
            // originating cell column from `col_index`.
            let char_start = row_buf[..match_byte_start].chars().count();
            let needle_char_len = needle_lower.chars().count();
            let char_end_exclusive = char_start + needle_char_len;
            // Defensive: needle_char_len > 0 because we returned early on
            // empty needle, so char_end_exclusive > char_start, so the
            // index col_index[char_end_exclusive - 1] is in range.
            let col_start = col_index[char_start];
            let col_end_inclusive = col_index[char_end_exclusive - 1];
            out.push(MatchRange {
                row: row_idx,
                col_start,
                col_end: col_end_inclusive + 1,
            });
            // Advance past this match by `needle_lower`'s byte length so
            // overlapping matches don't double-count.
            search_start = match_byte_start + needle_lower.len();
            if search_start >= row_buf.len() {
                break;
            }
        }
    }
    out
}

/// Bucket the flat `MatchRange` list by visible row.
///
/// `search_grid` is `history_len` history rows followed by `visible_rows`
/// visible rows. The renderer only needs match ranges for visible rows;
/// history matches are silently dropped (no scroll-to-match in v1). Returns
/// a `Vec` of length `visible_rows` where index `i` lists every match
/// `(col_start, col_end)` in the i-th visible row.
pub fn visible_match_ranges(
    matches: &[MatchRange],
    history_len: usize,
    visible_rows: usize,
) -> Vec<Vec<(usize, usize)>> {
    let mut buckets: Vec<Vec<(usize, usize)>> = vec![Vec::new(); visible_rows];
    for m in matches {
        if m.row < history_len {
            continue;
        }
        let visible_idx = m.row - history_len;
        if visible_idx >= visible_rows {
            continue;
        }
        buckets[visible_idx].push((m.col_start, m.col_end));
    }
    buckets
}

/// Bottom-right search overlay. Pure paint — no `.id()`, no `.occlude()`,
/// no event listeners — per the journal lesson on `.occlude()` blocking
/// hit-tests. Wrap the returned element inside the view via
/// `Div::child(render_search_overlay(...))`.
pub fn render_search_overlay(
    query: &str,
    theme: &Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .absolute()
        .bottom(px(12.0))
        .right(px(12.0))
        .px(px(10.0))
        .py(px(5.0))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        .text_color(theme.fg_base)
        .font_family(typography.family_mono.clone())
        .text_size(px(typography.t_body_lg))
        .child(SharedString::from(format!("/{query}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_pty::{Cell, CellColor};

    fn row_from_str(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|ch| Cell {
                ch,
                fg: CellColor::Default,
                bg: CellColor::Default,
                inverse: false,
            })
            .collect()
    }

    #[test]
    fn empty_needle_returns_no_matches() {
        let grid = vec![row_from_str("hello world")];
        assert!(find_matches(&grid, "").is_empty());
    }

    #[test]
    fn single_row_exact_match() {
        let grid = vec![row_from_str("hello")];
        let matches = find_matches(&grid, "hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 0);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 5);
    }

    #[test]
    fn case_insensitive() {
        let grid = vec![row_from_str("Hello World")];
        let matches = find_matches(&grid, "hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 5);
    }

    #[test]
    fn multiple_non_overlapping_matches_same_row() {
        let grid = vec![row_from_str("abcabc")];
        let matches = find_matches(&grid, "abc");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 3);
        assert_eq!(matches[1].col_start, 3);
        assert_eq!(matches[1].col_end, 6);
    }

    #[test]
    fn match_across_rows() {
        let grid = vec![
            row_from_str("nothing here"),
            row_from_str("needle in haystack"),
            row_from_str("also nothing"),
        ];
        let matches = find_matches(&grid, "needle");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 6);
    }

    #[test]
    fn no_match() {
        let grid = vec![row_from_str("abcdef")];
        assert!(find_matches(&grid, "xyz").is_empty());
    }

    #[test]
    fn visible_buckets_drop_history_only_matches() {
        let matches = vec![
            MatchRange {
                row: 0,
                col_start: 0,
                col_end: 5,
            }, // history
            MatchRange {
                row: 5,
                col_start: 2,
                col_end: 7,
            }, // visible row 0
            MatchRange {
                row: 6,
                col_start: 1,
                col_end: 3,
            }, // visible row 1
        ];
        let buckets = visible_match_ranges(&matches, 5, 2);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0], vec![(2, 7)]);
        assert_eq!(buckets[1], vec![(1, 3)]);
    }

    #[test]
    fn visible_buckets_skip_out_of_range_visible_rows() {
        // Match at row=10 with history_len=5 visible_rows=2 → visible_idx=5 → drop.
        let matches = vec![MatchRange {
            row: 10,
            col_start: 0,
            col_end: 1,
        }];
        let buckets = visible_match_ranges(&matches, 5, 2);
        assert!(buckets.iter().all(|b| b.is_empty()));
    }

    #[test]
    fn nul_cells_treated_as_space() {
        // Simulate a wide-char trail cell — we don't want the literal `\0`
        // to break needle alignment.
        let mut row = row_from_str("ab  cd");
        row[2].ch = '\0';
        row[3].ch = '\0';
        let grid = vec![row];
        let matches = find_matches(&grid, "ab  cd");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 6);
    }

    // --- QA-added edge cases ---

    /// Needle longer than any single row must not match.
    /// Guards against wrapping logic that might concatenate rows implicitly.
    #[test]
    fn needle_longer_than_row_returns_no_match() {
        let grid = vec![row_from_str("hello"), row_from_str(" world")];
        // "hello world" spans the row boundary — find_matches must NOT match it.
        assert!(find_matches(&grid, "hello world").is_empty());
    }

    /// Needle with internal whitespace must match literally within a row.
    #[test]
    fn needle_with_internal_whitespace_matches() {
        let grid = vec![row_from_str("foo bar baz")];
        let matches = find_matches(&grid, "foo bar");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 7);
        // "bar baz" at col 4..11
        let matches2 = find_matches(&grid, "bar baz");
        assert_eq!(matches2.len(), 1);
        assert_eq!(matches2[0].col_start, 4);
        assert_eq!(matches2[0].col_end, 11);
    }

    /// Non-overlapping guarantee: needle "aaa" in "aaaa" yields 1 match
    /// (col 0..3), NOT 2 (which would be the overlapping result at 0..3 and
    /// 1..4). Overlapping matches are explicitly excluded per doc comment.
    #[test]
    fn non_overlapping_guarantee_in_run_of_repeated_chars() {
        let grid = vec![row_from_str("aaaa")];
        let matches = find_matches(&grid, "aaa");
        // Advance past first match starts at byte 3, leaving only 1 char →
        // no second match.
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 3);
    }
}
