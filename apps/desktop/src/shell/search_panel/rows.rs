//! Flat row builder for the virtualized result list.
//!
//! Pure module — no GPUI imports — so the row interleaving logic stays
//! cheaply unit-testable.

use crate::shell::search_panel::rg_runner::SearchResults;
use std::collections::HashSet;
use std::path::PathBuf;

/// One row in the flat list rendered by `uniform_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRow {
    /// File header row. Indices reference back into `SearchResults.files`.
    File {
        file_index: usize,
        match_count: usize,
        collapsed: bool,
    },
    /// One match row inside a file. `match_index` is into `files[file_index].matches`.
    Match {
        file_index: usize,
        match_index: usize,
    },
}

/// Interleave file headers + match rows in the order required by the
/// virtualized list. Collapsed file paths drop their child matches but keep
/// the header row visible so the user can re-expand.
pub fn build_search_rows(
    results: Option<&SearchResults>,
    collapsed_files: &HashSet<PathBuf>,
) -> Vec<SearchRow> {
    let Some(r) = results else { return Vec::new() };

    let mut rows = Vec::with_capacity(r.files.len() + r.total_matches);
    for (file_index, file) in r.files.iter().enumerate() {
        let collapsed = collapsed_files.contains(&file.file_path);
        rows.push(SearchRow::File {
            file_index,
            match_count: file.matches.len(),
            collapsed,
        });
        if collapsed {
            continue;
        }
        for match_index in 0..file.matches.len() {
            rows.push(SearchRow::Match {
                file_index,
                match_index,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::search_panel::rg_runner::{SearchFileResult, SearchMatch};

    fn mk_match(line: u32, col: u32) -> SearchMatch {
        SearchMatch {
            line,
            column: col,
            line_content: "x".into(),
            match_length: 1,
        }
    }

    fn mk_file(path: &str, count: usize) -> SearchFileResult {
        SearchFileResult {
            file_path: PathBuf::from(path),
            relative_path: PathBuf::from(path.trim_start_matches("/repo/")),
            matches: (0..count).map(|i| mk_match(i as u32 + 1, 1)).collect(),
        }
    }

    #[test]
    fn none_results_yields_empty_rows() {
        let rows = build_search_rows(None, &HashSet::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn single_file_expands_into_header_plus_matches() {
        let r = SearchResults {
            files: vec![mk_file("/repo/a.rs", 2)],
            total_matches: 2,
            truncated: false,
        };
        let rows = build_search_rows(Some(&r), &HashSet::new());
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            rows[0],
            SearchRow::File {
                file_index: 0,
                match_count: 2,
                collapsed: false
            }
        ));
        assert!(matches!(
            rows[1],
            SearchRow::Match {
                file_index: 0,
                match_index: 0
            }
        ));
        assert!(matches!(
            rows[2],
            SearchRow::Match {
                file_index: 0,
                match_index: 1
            }
        ));
    }

    #[test]
    fn multi_file_interleaves_headers_and_matches() {
        let r = SearchResults {
            files: vec![mk_file("/repo/a.rs", 2), mk_file("/repo/b.rs", 1)],
            total_matches: 3,
            truncated: false,
        };
        let rows = build_search_rows(Some(&r), &HashSet::new());
        let kinds: Vec<&'static str> = rows
            .iter()
            .map(|r| match r {
                SearchRow::File { .. } => "f",
                SearchRow::Match { .. } => "m",
            })
            .collect();
        assert_eq!(kinds, vec!["f", "m", "m", "f", "m"]);
    }

    #[test]
    fn collapsed_file_drops_match_rows_but_keeps_header() {
        let r = SearchResults {
            files: vec![mk_file("/repo/a.rs", 2), mk_file("/repo/b.rs", 1)],
            total_matches: 3,
            truncated: false,
        };
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("/repo/a.rs"));
        let rows = build_search_rows(Some(&r), &collapsed);
        let kinds: Vec<&'static str> = rows
            .iter()
            .map(|r| match r {
                SearchRow::File { .. } => "f",
                SearchRow::Match { .. } => "m",
            })
            .collect();
        assert_eq!(kinds, vec!["f", "f", "m"]);

        // Collapsed flag must be reflected on the file row.
        if let SearchRow::File { collapsed, .. } = rows[0] {
            assert!(collapsed);
        } else {
            panic!("expected file row");
        }
    }

    #[test]
    fn all_files_collapsed_yields_headers_only() {
        let r = SearchResults {
            files: vec![mk_file("/repo/a.rs", 5), mk_file("/repo/b.rs", 3)],
            total_matches: 8,
            truncated: false,
        };
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("/repo/a.rs"));
        collapsed.insert(PathBuf::from("/repo/b.rs"));
        let rows = build_search_rows(Some(&r), &collapsed);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| matches!(r, SearchRow::File { .. })));
    }
}
