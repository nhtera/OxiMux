//! Tiny fuzzy scorer — prefix > consecutive substring > subsequence.
//!
//! Pure, no external crate. Sufficient for ≤200-entry stub catalogs; if
//! Phase N+1 wires a live file index we'll revisit (likely `nucleo-matcher`).

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
}
