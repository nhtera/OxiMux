//! `filter_files` covers empty/whitespace queries, case-insensitive substrings,
//! and multi-path nested matches.

use oximux_app::shell::source_control::filter::filter_files;
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::path::PathBuf;

fn fs(path: &str) -> FileStatus {
    FileStatus {
        path: PathBuf::from(path),
        index: IndexStatus::Modified,
        worktree: WorktreeStatus::Unmodified,
        rename: None,
    }
}

#[test]
fn empty_query_returns_everything() {
    let files = vec![fs("a.rs"), fs("b.rs")];
    assert_eq!(filter_files(&files, "").len(), 2);
    assert_eq!(filter_files(&files, "   ").len(), 2);
}

#[test]
fn substring_match_keeps_only_matches() {
    let files = vec![fs("src/main.rs"), fs("src/lib.rs"), fs("Cargo.toml")];
    let kept = filter_files(&files, "src");
    assert_eq!(kept.len(), 2);
    assert!(
        kept.iter()
            .any(|f| f.path.as_path() == std::path::Path::new("src/main.rs"))
    );
    assert!(
        kept.iter()
            .any(|f| f.path.as_path() == std::path::Path::new("src/lib.rs"))
    );
}

#[test]
fn case_insensitive_match() {
    let files = vec![fs("README.md")];
    assert_eq!(filter_files(&files, "readme").len(), 1);
    assert_eq!(filter_files(&files, "READ").len(), 1);
    assert_eq!(filter_files(&files, "ReAdMe").len(), 1);
}

#[test]
fn no_match_returns_empty() {
    let files = vec![fs("src/main.rs")];
    assert!(filter_files(&files, "doesnotexist").is_empty());
}

#[test]
fn partial_path_segment_matches() {
    let files = vec![fs("crates/git/src/log.rs"), fs("crates/app/src/main.rs")];
    let kept = filter_files(&files, "git");
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].path.as_path(),
        std::path::Path::new("crates/git/src/log.rs")
    );
}
