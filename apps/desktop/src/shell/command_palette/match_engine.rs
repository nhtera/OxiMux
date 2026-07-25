//! Tiny fuzzy scorer — prefix > consecutive substring > subsequence.
//!
//! Pure, no external crate. Sufficient for ≤200-entry stub catalogs; if
//! Phase N+1 wires a live file index we'll revisit (likely `nucleo-matcher`).

use std::ops::Range;

/// Score a single candidate against a query.
/// - `100` prefix match
/// - ` 60` consecutive substring
/// - ` 20` scattered subsequence
/// - ` -1` no match
/// - `  0` empty query (caller should return all in original order)
pub fn score(query: &str, candidate: &str) -> i32 {
    if query.is_empty() {
        return 0;
    }
    let q = query.to_lowercase();
    let c = candidate.to_lowercase();
    if c.starts_with(&q) {
        return 100;
    }
    if c.contains(&q) {
        return 60;
    }
    let mut qi = q.chars().peekable();
    for ch in c.chars() {
        if qi.peek() == Some(&ch) {
            qi.next();
        }
    }
    if qi.peek().is_none() { 20 } else { -1 }
}

/// Filter + rank candidates against `query`. Returns the original indices
/// in score-descending order. Empty query returns all in original order.
pub fn filter_and_rank(query: &str, candidates: &[&str]) -> Vec<usize> {
    if query.is_empty() {
        return (0..candidates.len()).collect();
    }
    let mut scored: Vec<(i32, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let s = score(query, c);
            if s >= 0 { Some((s, i)) } else { None }
        })
        .collect();
    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Byte ranges in `candidate` that the `query` matched, for highlight
/// rendering in the palette. Mirrors [`score`]'s matching order:
/// prefix/substring collapse to one contiguous range; a scattered
/// subsequence yields one range per matched char. Empty when the query is
/// empty or there's no match.
///
/// Offsets index the ORIGINAL-cased `candidate` so callers can hand them
/// straight to a styled-text run. To stay correct under case folding the
/// contiguous path is only taken when lowercasing preserves byte length
/// (always true for ASCII command names); otherwise it falls through to the
/// per-char scan, which tracks original-string byte offsets directly.
pub fn match_ranges(query: &str, candidate: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    let lower = candidate.to_lowercase();
    if candidate.len() == lower.len()
        && let Some(start) = lower.find(&q)
    {
        // Single contiguous hit (prefix/substring) — one range by design, not
        // an accidental single-element init.
        #[allow(clippy::single_range_in_vec_init)]
        return vec![start..start + q.len()];
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut q_chars = q.chars().peekable();
    let mut byte = 0usize;
    for ch in candidate.chars() {
        let len = ch.len_utf8();
        if let Some(&qc) = q_chars.peek()
            && ch.to_lowercase().next() == Some(qc)
        {
            ranges.push(byte..byte + len);
            q_chars.next();
        }
        byte += len;
    }
    // Incomplete consume → not actually a match; emit nothing rather than a
    // misleading partial highlight.
    if q_chars.peek().is_some() {
        return Vec::new();
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_prefix_match_above_substring() {
        // "main.rs" is a prefix match for "ma" → 100; "src/main.rs" is substring → 60.
        assert!(score("ma", "main.rs") > score("ma", "src/main.rs"));
    }

    #[test]
    fn empty_query_returns_all_in_original_order() {
        assert_eq!(filter_and_rank("", &["b", "a", "c"]), vec![0, 1, 2]);
    }

    #[test]
    fn case_insensitive_default() {
        assert_eq!(score("MA", "main.rs"), score("ma", "main.rs"));
    }

    #[test]
    fn subsequence_match_scores_lowest() {
        // "as" appears in "main.rs" only as a subsequence (a…s), not a substring.
        assert_eq!(score("as", "main.rs"), 20);
    }

    #[test]
    fn no_match_returns_negative() {
        assert_eq!(score("xyz", "main.rs"), -1);
    }

    #[test]
    fn filter_excludes_non_matches() {
        let r = filter_and_rank("xyz", &["main.rs", "lib.rs"]);
        assert!(r.is_empty());
    }

    #[test]
    fn match_ranges_empty_query_is_empty() {
        assert!(match_ranges("", "Split Pane").is_empty());
    }

    #[test]
    fn match_ranges_prefix_is_contiguous() {
        // "spl" prefixes "Split Pane" → one range covering the first 3 bytes.
        assert_eq!(match_ranges("spl", "Split Pane"), vec![0..3]);
    }

    #[test]
    fn match_ranges_substring_is_contiguous() {
        // "pane" is a substring starting at byte 6 of "Split Pane".
        assert_eq!(match_ranges("pane", "Split Pane"), vec![6..10]);
    }

    #[test]
    fn match_ranges_subsequence_is_per_char() {
        // "sc" is not a substring of "Source Control" (no adjacent s-c), so it
        // matches as a scattered subsequence: S(0) then the first later 'c',
        // which is "sourCe" at index 4 (greedy, before "Control"'s C).
        assert_eq!(match_ranges("sc", "Source Control"), vec![0..1, 4..5]);
    }

    #[test]
    fn match_ranges_no_match_is_empty() {
        assert!(match_ranges("zzz", "Split Pane").is_empty());
    }

    #[test]
    fn match_ranges_align_with_score_matchability() {
        // Anything `score` accepts (>= 0, non-empty query) should yield a
        // non-empty highlight, and anything it rejects yields none.
        for cand in ["New Tab", "Quick Open", "Toggle Left Sidebar"] {
            for q in ["new", "qo", "tls", "tab"] {
                let matched = score(q, cand) >= 0;
                assert_eq!(matched, !match_ranges(q, cand).is_empty(), "q={q} cand={cand}");
            }
        }
    }
}
