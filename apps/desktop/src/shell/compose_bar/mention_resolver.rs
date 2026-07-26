//! File-list scanning + fuzzy ranking for the `@file` autocomplete dropdown.
//!
//! The candidate list is scanned once (rg-backed, capped at the index limit)
//! and cached by the composer; ranking then runs purely on each keystroke. If
//! `rg` is unavailable the scan yields an empty list — the dropdown simply
//! shows nothing rather than panicking.

use std::path::PathBuf;

use crate::shell::command_palette::file_index::scan_files;
use crate::shell::command_palette::match_engine::filter_and_rank;

/// Maximum number of suggestion rows shown in the dropdown.
pub const MAX_SUGGESTIONS: usize = 8;

/// Scan `root` for candidate files. Any failure (notably `rg` missing) maps to
/// an empty list so the composer never panics and the dropdown degrades
/// gracefully. The cap and `.gitignore` handling come from `scan_files`.
pub async fn scan_candidates(root: PathBuf) -> Vec<String> {
    match scan_files(root).await {
        Ok(files) => files,
        Err(err) => {
            tracing::debug!(?err, "compose @mention scan failed — empty candidate list");
            Vec::new()
        }
    }
}

/// Rank `candidates` against `query`, returning up to `limit` paths best-first.
/// An empty query returns the first `limit` candidates in scan order so the
/// dropdown is immediately useful the moment `@` is typed.
pub fn rank(candidates: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.is_empty() {
        return candidates.iter().take(limit).cloned().collect();
    }
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    filter_and_rank(query, &refs)
        .into_iter()
        .take(limit)
        .map(|i| candidates[i].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<String> {
        vec![
            "src/auth.rs".to_string(),
            "src/main.rs".to_string(),
            "README.md".to_string(),
            "crates/app/src/lib.rs".to_string(),
        ]
    }

    #[test]
    fn empty_query_returns_scan_order_capped() {
        let c = sample();
        let out = rank(&c, "", 2);
        assert_eq!(out, vec!["src/auth.rs".to_string(), "src/main.rs".to_string()]);
    }

    #[test]
    fn query_ranks_by_fuzzy_match() {
        let c = sample();
        let out = rank(&c, "auth", MAX_SUGGESTIONS);
        assert_eq!(out.first().map(String::as_str), Some("src/auth.rs"));
    }

    #[test]
    fn no_match_returns_empty() {
        let c = sample();
        assert!(rank(&c, "zzzznotathing", MAX_SUGGESTIONS).is_empty());
    }
}
