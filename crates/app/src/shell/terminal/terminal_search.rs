//! Scrollback search — Phase 1 step 8.
//!
//! Pure scan over a row-major cell grid. The grid is whatever
//! `TerminalBackend::search_grid` hands back (history rows + visible rows
//! today; could be just visible rows on backends that don't retain history).
//!
//! Three independent toggles ride on the same scan: `case_sensitive`,
//! `whole_word`, `regex`. The base path is a plain substring scan; regex
//! mode dispatches to the `regex` crate (with `(?i)` prepended when case-
//! insensitive). Whole-word post-filters either path with an ASCII
//! word-boundary check on the adjacent cells.
//!
//! Lowercasing happens once on the needle and once per row in non-regex
//! mode. Allocations are intentionally not pooled — the grid is bounded at
//! ~400k cells (5000 history + ~32 rows × 200 cols) and a flood test
//! against `yes` still scans under 1 ms.

use oximux_pty::Cell;
use regex::RegexBuilder;

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

/// Editor-style search toggles. All default to `false`, which preserves the
/// original "case-insensitive plain substring" behavior of the find overlay.
///
/// - `case_sensitive` — when true, both row and needle keep their original
///   case; when false, both are lowercased before substring/regex match.
/// - `whole_word` — post-filter that requires the adjacent cells on either
///   side of the match to be non-word characters (ASCII word = `[A-Za-z0-9_]`).
/// - `regex` — switch the scan engine from plain substring to `regex` crate.
///   Invalid patterns are silently treated as "no matches" so the overlay
///   doesn't error-spam mid-typing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Scan the grid for `needle` under the given `options`. Returns all
/// non-overlapping matches in top-to-bottom, left-to-right order. Empty
/// needle → empty result.
///
/// Wide-char (CJK) caveat: alacritty represents wide cells as the glyph in
/// one cell + `\0` in the next. We treat `\0` cells as spaces here so they
/// don't poison the row string with literal nuls; cost is that a CJK glyph
/// won't match its romaji input, which is acceptable for v1 (English-first
/// terminal content). When CJK input matters, switch to a grapheme-aware
/// scanner.
pub fn find_matches_with_options(
    grid: &[Vec<Cell>],
    needle: &str,
    options: SearchOptions,
) -> Vec<MatchRange> {
    if needle.is_empty() {
        return Vec::new();
    }
    if options.regex {
        scan_regex(grid, needle, options)
    } else {
        scan_substring(grid, needle, options)
    }
}

/// Plain-substring scan. Lowercases the needle once when case-insensitive,
/// then lowercases each row into a scratch buffer and uses `str::find`.
fn scan_substring(grid: &[Vec<Cell>], needle: &str, options: SearchOptions) -> Vec<MatchRange> {
    let needle_norm = normalize_case(needle, options.case_sensitive);
    let mut out = Vec::new();
    let mut row_buf = String::with_capacity(256);
    let mut col_index = Vec::with_capacity(256);

    for (row_idx, row) in grid.iter().enumerate() {
        row_buf.clear();
        col_index.clear();
        build_row_buf(row, options.case_sensitive, &mut row_buf, &mut col_index);

        let mut search_start = 0;
        while let Some(found_byte) = row_buf[search_start..].find(&needle_norm) {
            let match_byte_start = search_start + found_byte;
            let char_start = row_buf[..match_byte_start].chars().count();
            let needle_char_len = needle_norm.chars().count();
            if needle_char_len == 0 {
                // Mirrors the regex path's zero-width guard: nothing
                // highlightable, and `char_end_exclusive - 1` would
                // underflow below.
                break;
            }
            let char_end_exclusive = char_start + needle_char_len;
            // In-bounds by construction (col_index has one entry per
            // row_buf char), but a `get` keeps a future drift in the
            // buf/index pairing a missed match instead of a panic.
            let (Some(&col_start), Some(&col_end_inclusive)) = (
                col_index.get(char_start),
                col_index.get(char_end_exclusive - 1),
            ) else {
                break;
            };
            let col_end = col_end_inclusive + 1;
            if !options.whole_word || is_word_boundary(row, col_start, col_end) {
                out.push(MatchRange {
                    row: row_idx,
                    col_start,
                    col_end,
                });
            }
            search_start = match_byte_start + needle_norm.len();
            if search_start >= row_buf.len() {
                break;
            }
        }
    }
    out
}

/// Compile a search regex with the overlay's case toggle applied. `None`
/// for invalid patterns (silent — the overlay surfaces errors visually
/// rather than panicking mid-typing). Exposed so the stateful search can
/// cache the compiled regex across reruns of the same needle.
pub fn compile_search_regex(needle: &str, case_sensitive: bool) -> Option<regex::Regex> {
    RegexBuilder::new(needle)
        .case_insensitive(!case_sensitive)
        .build()
        .ok()
}

/// Regex scan against a caller-supplied precompiled regex. Split from
/// [`scan_regex`] so a cached regex skips recompilation per rerun.
pub fn find_matches_precompiled(
    grid: &[Vec<Cell>],
    re: &regex::Regex,
    options: SearchOptions,
) -> Vec<MatchRange> {
    scan_regex_with(grid, re, options)
}

/// Regex scan. Uses `RegexBuilder` so we can flip `case_insensitive`
/// rather than splicing `(?i)` into the pattern (cleaner, and works with
/// anchors).
fn scan_regex(grid: &[Vec<Cell>], needle: &str, options: SearchOptions) -> Vec<MatchRange> {
    let Some(re) = compile_search_regex(needle, options.case_sensitive) else {
        return Vec::new();
    };
    scan_regex_with(grid, &re, options)
}

fn scan_regex_with(grid: &[Vec<Cell>], re: &regex::Regex, options: SearchOptions) -> Vec<MatchRange> {
    let mut out = Vec::new();
    let mut row_buf = String::with_capacity(256);
    let mut col_index = Vec::with_capacity(256);
    for (row_idx, row) in grid.iter().enumerate() {
        row_buf.clear();
        col_index.clear();
        // Regex carries case-insensitivity itself; build the row in its
        // original case so `^`/`$`/look-arounds report the right bytes.
        build_row_buf(row, true, &mut row_buf, &mut col_index);
        for m in re.find_iter(&row_buf) {
            let char_start = row_buf[..m.start()].chars().count();
            let match_chars = row_buf[m.start()..m.end()].chars().count();
            if match_chars == 0 {
                // `.*` and friends will match empty positions on every row;
                // those aren't highlightable, so skip rather than emit a
                // zero-width range that the renderer would have to special-
                // case.
                continue;
            }
            let char_end_exclusive = char_start + match_chars;
            // Same defensive `get` as the substring path — see comment there.
            let (Some(&col_start), Some(&col_end_inclusive)) = (
                col_index.get(char_start),
                col_index.get(char_end_exclusive - 1),
            ) else {
                continue;
            };
            let col_end = col_end_inclusive + 1;
            if !options.whole_word || is_word_boundary(row, col_start, col_end) {
                out.push(MatchRange {
                    row: row_idx,
                    col_start,
                    col_end,
                });
            }
        }
    }
    out
}

/// Build the per-row scratch string + char→column index. When
/// `keep_case` is false the chars are lowercased (one char in, possibly
/// multiple chars out — every output char maps back to the same source
/// column).
fn build_row_buf(row: &[Cell], keep_case: bool, row_buf: &mut String, col_index: &mut Vec<usize>) {
    for (col_idx, cell) in row.iter().enumerate() {
        let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
        if keep_case {
            row_buf.push(ch);
            col_index.push(col_idx);
        } else {
            for lowered in ch.to_lowercase() {
                row_buf.push(lowered);
                col_index.push(col_idx);
            }
        }
    }
}

fn normalize_case(s: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        s.to_string()
    } else {
        s.to_lowercase()
    }
}

/// Is the match span `[col_start, col_end)` flanked by non-word characters
/// (or row edges)? ASCII word = `[A-Za-z0-9_]`. Anything else — including
/// the `\0` trail-cells from wide chars — counts as a boundary.
fn is_word_boundary(row: &[Cell], col_start: usize, col_end: usize) -> bool {
    let left_ok = col_start == 0
        || row
            .get(col_start - 1)
            .map(|c| !is_word_char(c.ch))
            .unwrap_or(true);
    let right_ok = col_end >= row.len()
        || row
            .get(col_end)
            .map(|c| !is_word_char(c.ch))
            .unwrap_or(true);
    left_ok && right_ok
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
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
                ..Cell::default()
            })
            .collect()
    }

    /// Test helper — default-options scan (case-insensitive substring,
    /// no whole-word, no regex). Mirrors the pre-toggle behavior so the
    /// legacy tests stay readable.
    fn find(grid: &[Vec<Cell>], needle: &str) -> Vec<MatchRange> {
        find_matches_with_options(grid, needle, SearchOptions::default())
    }

    /// Test helper — same scan but with explicit options for the toggle
    /// tests below.
    fn find_opts(grid: &[Vec<Cell>], needle: &str, opts: SearchOptions) -> Vec<MatchRange> {
        find_matches_with_options(grid, needle, opts)
    }

    #[test]
    fn empty_needle_returns_no_matches() {
        let grid = vec![row_from_str("hello world")];
        assert!(find(&grid, "").is_empty());
    }

    #[test]
    fn single_row_exact_match() {
        let grid = vec![row_from_str("hello")];
        let matches = find(&grid, "hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 0);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 5);
    }

    #[test]
    fn case_insensitive() {
        let grid = vec![row_from_str("Hello World")];
        let matches = find(&grid, "hello");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 5);
    }

    #[test]
    fn multiple_non_overlapping_matches_same_row() {
        let grid = vec![row_from_str("abcabc")];
        let matches = find(&grid, "abc");
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
        let matches = find(&grid, "needle");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 6);
    }

    #[test]
    fn no_match() {
        let grid = vec![row_from_str("abcdef")];
        assert!(find(&grid, "xyz").is_empty());
    }

    #[test]
    fn nul_cells_treated_as_space() {
        // Simulate a wide-char trail cell — we don't want the literal `\0`
        // to break needle alignment.
        let mut row = row_from_str("ab  cd");
        row[2].ch = '\0';
        row[3].ch = '\0';
        let grid = vec![row];
        let matches = find(&grid, "ab  cd");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 6);
    }

    /// Unicode fixture: 'İ' lowercases to TWO chars (i + combining dot),
    /// so the case-insensitive scratch buffer has more chars than the row
    /// has columns — exercising the char→column index where one source
    /// column owns several scratch chars. The wide glyph + '\0' trail
    /// cell exercises the trail-cell mapping, and the row-final match
    /// exercises the `col_end_inclusive + 1` edge. Locks in: in-bounds,
    /// no panic, column-space spans.
    #[test]
    fn unicode_expanding_lowercase_and_wide_glyphs_stay_in_bounds() {
        // Columns: İ(0) s(1) t(2) 漢(3) trail(4) a(5) b(6) c(7)
        let mut row = row_from_str("İst漢 abc");
        row[4].ch = '\0';
        let grid = vec![row];
        // Case-insensitive: needle and row go through the same expansion.
        let matches = find(&grid, "İst");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 3);
        // Match ending at the very last column.
        let matches = find(&grid, "abc");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 5);
        assert_eq!(matches[0].col_end, 8);
        // Regex path over the same row must stay in bounds too.
        let matches = find_opts(
            &grid,
            "a.c",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_end, 8);
    }

    // --- QA-added edge cases ---

    /// Needle longer than any single row must not match.
    /// Guards against wrapping logic that might concatenate rows implicitly.
    #[test]
    fn needle_longer_than_row_returns_no_match() {
        let grid = vec![row_from_str("hello"), row_from_str(" world")];
        // "hello world" spans the row boundary — find_matches must NOT match it.
        assert!(find(&grid, "hello world").is_empty());
    }

    /// Needle with internal whitespace must match literally within a row.
    #[test]
    fn needle_with_internal_whitespace_matches() {
        let grid = vec![row_from_str("foo bar baz")];
        let matches = find(&grid, "foo bar");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 7);
        // "bar baz" at col 4..11
        let matches2 = find(&grid, "bar baz");
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
        let matches = find(&grid, "aaa");
        // Advance past first match starts at byte 3, leaving only 1 char →
        // no second match.
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 3);
    }

    // --- Toggle behaviors (Aa / ab / .*) ---

    #[test]
    fn case_sensitive_toggle_excludes_other_case() {
        let grid = vec![row_from_str("Hello hello HELLO")];
        let opts = SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, "hello", opts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 6);
        assert_eq!(matches[0].col_end, 11);
    }

    #[test]
    fn whole_word_toggle_filters_substring_hits() {
        let grid = vec![row_from_str("foo foobar barfoo")];
        let opts = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, "foo", opts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 3);
    }

    #[test]
    fn whole_word_treats_underscore_as_word_char() {
        let grid = vec![row_from_str("_foo_ foo ")];
        let opts = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, "foo", opts);
        // Only the standalone "foo" passes; "_foo_" is rejected because
        // underscore is a word char.
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 6);
        assert_eq!(matches[0].col_end, 9);
    }

    #[test]
    fn regex_toggle_matches_alternation() {
        let grid = vec![row_from_str("error: panic: warn:")];
        let opts = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, r"(error|panic):", opts);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].col_start, 0);
        assert_eq!(matches[0].col_end, 6);
        assert_eq!(matches[1].col_start, 7);
        assert_eq!(matches[1].col_end, 13);
    }

    #[test]
    fn regex_invalid_pattern_yields_no_matches() {
        // Unclosed group — would error at compile. We silently emit zero
        // matches so the overlay can keep accepting keystrokes.
        let grid = vec![row_from_str("anything goes")];
        let opts = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        assert!(find_opts(&grid, "(unclosed", opts).is_empty());
    }

    #[test]
    fn regex_respects_case_sensitive_toggle() {
        let grid = vec![row_from_str("Foo foo FOO")];
        let opts = SearchOptions {
            case_sensitive: true,
            regex: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, r"foo", opts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].col_start, 4);
        assert_eq!(matches[0].col_end, 7);
    }

    #[test]
    fn regex_zero_width_match_is_skipped() {
        // `.*` would match at the start of every row at width zero. The
        // renderer can't paint zero-width highlights, so the scan drops
        // those silently.
        let grid = vec![row_from_str("abc")];
        let opts = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let matches = find_opts(&grid, r"^", opts);
        assert!(matches.is_empty());
    }
}
