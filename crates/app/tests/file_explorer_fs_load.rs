//! Integration tests for `file_explorer::fs_load::read_dir_filtered`.
//!
//! Builds tempdir fixtures and asserts: excluded names dropped, dirs-first
//! sort, case-insensitive ordering, `relative_path` computed from `repo_root`.

use std::fs;
use std::path::PathBuf;

use oximux_app::shell::file_explorer::fs_load::read_dir_filtered;

/// Build a fixture tree under `root`:
///   .git/HEAD
///   node_modules/foo
///   target/debug
///   src/  (dir)
///   docs/ (dir)
///   README.md
///   .gitignore
fn build_fixture(root: &std::path::Path) {
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/foo"), "").unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("README.md"), "# fixture\n").unwrap();
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
}

#[tokio::test]
async fn read_dir_filtered_skips_excluded_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    build_fixture(tmp.path());

    let nodes = read_dir_filtered(tmp.path().to_path_buf(), tmp.path().to_path_buf())
        .await
        .unwrap();

    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(!names.contains(&".git"), "names: {names:?}");
    assert!(!names.contains(&"node_modules"), "names: {names:?}");
    assert!(!names.contains(&"target"), "names: {names:?}");
}

#[tokio::test]
async fn read_dir_filtered_sorts_dirs_first_case_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    build_fixture(tmp.path());

    let nodes = read_dir_filtered(tmp.path().to_path_buf(), tmp.path().to_path_buf())
        .await
        .unwrap();

    // Expected ordering: dirs first (docs, src), then files (.gitignore, README.md)
    // case-insensitive => .gitignore before README.md (dot doesn't change order; "g" < "r")
    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["docs", "src", ".gitignore", "README.md"]);

    // All dir entries come before any file entry.
    let first_file_idx = nodes.iter().position(|n| !n.is_directory).unwrap();
    assert!(
        nodes[..first_file_idx].iter().all(|n| n.is_directory),
        "non-dir found before first_file_idx"
    );
}

#[tokio::test]
async fn read_dir_filtered_computes_relative_path_from_repo_root() {
    let tmp = tempfile::tempdir().unwrap();
    build_fixture(tmp.path());

    let repo_root = tmp.path().to_path_buf();
    let nodes = read_dir_filtered(tmp.path().join("src"), repo_root.clone())
        .await
        .unwrap();
    // src/ is empty in the fixture — that's fine, the contract is about the
    // function not panicking on an empty dir and respecting repo_root.
    assert!(nodes.is_empty());

    // Now read the parent and check relative_path on one entry.
    let parent = read_dir_filtered(repo_root.clone(), repo_root.clone())
        .await
        .unwrap();
    let readme = parent.iter().find(|n| n.name == "README.md").unwrap();
    assert_eq!(readme.relative_path, PathBuf::from("README.md"));
    let src = parent.iter().find(|n| n.name == "src").unwrap();
    assert_eq!(src.relative_path, PathBuf::from("src"));
}

#[tokio::test]
async fn read_dir_filtered_depth_is_zero() {
    let tmp = tempfile::tempdir().unwrap();
    build_fixture(tmp.path());

    let nodes = read_dir_filtered(tmp.path().to_path_buf(), tmp.path().to_path_buf())
        .await
        .unwrap();

    assert!(nodes.iter().all(|n| n.depth == 0));
}

#[tokio::test]
async fn read_dir_filtered_errors_on_missing_path() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");

    let err = read_dir_filtered(missing, tmp.path().to_path_buf())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
