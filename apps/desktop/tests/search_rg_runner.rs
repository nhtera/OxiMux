//! Integration test: shell out to real `ripgrep` against the checked-in
//! fixture directory under `tests/fixtures/search/`.
//!
//! Skips automatically when `rg` is not on PATH so CI environments without
//! the binary don't fail (they should run `apt-get install ripgrep` etc).

use oximux_app::shell::search_panel::rg_runner::{
    DEFAULT_MAX_RESULTS, detect_rg_available, run_ripgrep,
};
use oximux_app::shell::search_panel::search_state::SearchOptions;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search")
}

#[tokio::test]
async fn rg_available_on_dev_machine() {
    // Sanity probe — if this fails, every other test below auto-skips.
    let _ = detect_rg_available().await;
}

#[tokio::test]
async fn finds_filestatus_across_files() {
    if !detect_rg_available().await {
        eprintln!("ripgrep not on PATH — skipping");
        return;
    }
    let opts = SearchOptions {
        query: "FileStatus".into(),
        ..Default::default()
    };
    let r = run_ripgrep(fixture_root(), opts, DEFAULT_MAX_RESULTS)
        .await
        .expect("rg ran ok");
    assert!(
        r.files.len() >= 2,
        "expected at least 2 files, got {}",
        r.files.len()
    );
    assert!(r.total_matches >= 2);
    // Every match should record the byte column (1-based).
    for f in &r.files {
        for m in &f.matches {
            assert!(m.column >= 1);
            assert!(m.match_length > 0);
        }
    }
}

/// Regression for issue #73: rg reports paths relative to its working
/// directory, and a relative `file_path` made every result click open
/// nothing. Each result must carry an absolute path to a real file, with
/// `relative_path` still relative for display.
#[tokio::test]
async fn result_paths_are_absolute_and_exist() {
    if !detect_rg_available().await {
        eprintln!("ripgrep not on PATH — skipping");
        return;
    }
    let opts = SearchOptions {
        query: "FileStatus".into(),
        ..Default::default()
    };
    let root = fixture_root();
    let r = run_ripgrep(root.clone(), opts, DEFAULT_MAX_RESULTS)
        .await
        .expect("rg ran ok");
    assert!(!r.files.is_empty());
    for f in &r.files {
        assert!(f.file_path.is_absolute(), "not absolute: {}", f.file_path.display());
        assert!(f.file_path.is_file(), "missing: {}", f.file_path.display());
        assert!(f.relative_path.is_relative(), "{}", f.relative_path.display());
        assert_eq!(root.join(&f.relative_path), f.file_path);
    }
}

#[tokio::test]
async fn case_sensitive_excludes_lowercase_hits() {
    if !detect_rg_available().await {
        return;
    }
    let opts = SearchOptions {
        query: "Hello".into(),
        case_sensitive: true,
        ..Default::default()
    };
    let r = run_ripgrep(fixture_root(), opts, DEFAULT_MAX_RESULTS)
        .await
        .expect("rg ran");
    // Fixture has "hello" lowercase only.
    assert_eq!(r.total_matches, 0, "expected 0 case-sensitive matches");
}

#[tokio::test]
async fn glob_filter_restricts_to_subdir() {
    if !detect_rg_available().await {
        return;
    }
    let opts = SearchOptions {
        query: "FileStatus".into(),
        include_glob: "sub/**".into(),
        ..Default::default()
    };
    let r = run_ripgrep(fixture_root(), opts, DEFAULT_MAX_RESULTS)
        .await
        .expect("rg ran");
    assert_eq!(r.files.len(), 1);
    assert!(
        r.files[0].relative_path.starts_with("sub"),
        "expected hit inside sub/, got {:?}",
        r.files[0].relative_path
    );
}

#[tokio::test]
async fn empty_query_returns_empty_results() {
    let opts = SearchOptions {
        query: "   ".into(),
        ..Default::default()
    };
    let r = run_ripgrep(fixture_root(), opts, DEFAULT_MAX_RESULTS)
        .await
        .expect("empty query ok");
    assert_eq!(r.total_matches, 0);
    assert!(r.files.is_empty());
}

#[tokio::test]
async fn truncates_when_cap_hit() {
    if !detect_rg_available().await {
        return;
    }
    // Query matches every line containing "e" — fixture has many.
    let opts = SearchOptions {
        query: "e".into(),
        use_regex: true,
        ..Default::default()
    };
    let r = run_ripgrep(fixture_root(), opts, 2).await.expect("rg ran");
    assert!(r.total_matches >= 2);
    assert!(r.truncated, "expected truncated=true at cap=2");
}
