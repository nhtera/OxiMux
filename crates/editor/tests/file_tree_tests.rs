//! Integration tests for the file_tree backend.
//!
//! `TempDir` setup creates a `.git/` marker so `WalkBuilder::git_ignore`
//! engages (the `ignore` crate looks for `.git` to decide if a directory
//! is a repo root). Without that marker, `.gitignore` rules are silently
//! ignored — exactly the kind of test/prod divergence the parent plan
//! warns against.

use oximux_editor::file_tree::walker::{list_children, sort_entries};
use std::{fs, path::PathBuf, time::Instant};
use tempfile::TempDir;

fn fake_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join(".git")).expect("mk .git");
    dir
}

fn names(entries: &[(PathBuf, bool)]) -> Vec<String> {
    entries
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn walker_lists_direct_children() {
    let dir = fake_repo();
    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::write(dir.path().join("b.rs"), "").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub").join("inner.rs"), "").unwrap();

    let mut entries = list_children(dir.path());
    sort_entries(&mut entries);
    let n = names(&entries);
    assert_eq!(n, vec!["sub", "a.rs", "b.rs"]);
}

#[test]
fn walker_skips_git_dir() {
    let dir = fake_repo();
    fs::write(dir.path().join("real.rs"), "").unwrap();
    let entries = list_children(dir.path());
    let n = names(&entries);
    assert!(!n.iter().any(|x| x == ".git"), "got {n:?}");
    assert!(n.iter().any(|x| x == "real.rs"));
}

#[test]
fn walker_respects_gitignore() {
    let dir = fake_repo();
    fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(dir.path().join("keep.rs"), "").unwrap();
    fs::write(dir.path().join("drop.log"), "").unwrap();

    let entries = list_children(dir.path());
    let n = names(&entries);
    assert!(n.iter().any(|x| x == "keep.rs"));
    assert!(
        !n.iter().any(|x| x == "drop.log"),
        "log not filtered: {n:?}"
    );
}

#[test]
fn walker_skips_node_modules_and_target() {
    let dir = fake_repo();
    fs::create_dir(dir.path().join("node_modules")).unwrap();
    fs::create_dir(dir.path().join("target")).unwrap();
    fs::write(dir.path().join("src.rs"), "").unwrap();

    let entries = list_children(dir.path());
    let n = names(&entries);
    assert!(!n.iter().any(|x| x == "node_modules"));
    assert!(!n.iter().any(|x| x == "target"));
    assert!(n.iter().any(|x| x == "src.rs"));
}

#[test]
#[cfg(unix)]
fn walker_symlinks_as_leaf() {
    use std::os::unix::fs::symlink;
    let dir = fake_repo();
    fs::create_dir(dir.path().join("real_dir")).unwrap();
    fs::write(dir.path().join("real_dir").join("inside.rs"), "").unwrap();
    symlink(dir.path().join("real_dir"), dir.path().join("link")).unwrap();

    let entries = list_children(dir.path());
    let n = names(&entries);
    assert!(n.iter().any(|x| x == "link"), "symlink missing: {n:?}");
    // We do not traverse the symlink — `inside.rs` must not appear at root.
    assert!(!n.iter().any(|x| x == "inside.rs"));
}

#[test]
fn walker_empty_dir_returns_empty() {
    let dir = fake_repo();
    let entries = list_children(dir.path());
    assert!(
        entries.is_empty(),
        "expected empty, got {:?}",
        names(&entries)
    );
}

#[test]
fn walker_perf_1000_files() {
    let dir = fake_repo();
    for i in 0..1000 {
        fs::write(dir.path().join(format!("f{i}.rs")), "").unwrap();
    }
    let started = Instant::now();
    let entries = list_children(dir.path());
    let elapsed = started.elapsed();
    eprintln!("walker_perf_1000_files: {elapsed:?}");
    assert_eq!(entries.len(), 1000);
    assert!(
        elapsed.as_millis() < 200,
        "walker too slow: {elapsed:?} for 1000 files"
    );
    // Real budget is < 50ms on SSD; 200ms ceiling catches catastrophic
    // regressions while tolerating CI machine variability.
}
