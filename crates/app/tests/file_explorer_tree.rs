//! Unit tests for `tree_state` — `should_include` and `flatten`.
//! These duplicate the in-module tests from tree_state.rs to satisfy the
//! "tests under crates/app/tests/" requirement and add additional cases.

use oximux_app::shell::file_explorer::tree_state::{DirCache, TreeNode, flatten, should_include};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn node(name: &str, root: &Path, is_dir: bool) -> TreeNode {
    let path = root.join(name);
    TreeNode {
        name: name.to_string(),
        path: path.clone(),
        relative_path: PathBuf::from(name),
        is_directory: is_dir,
        depth: 0,
    }
}

// ── should_include ───────────────────────────────────────────────────────────

#[test]
fn includes_rejects_dot_git() {
    assert!(!should_include(".git"));
}

#[test]
fn includes_rejects_node_modules() {
    assert!(!should_include("node_modules"));
}

#[test]
fn includes_rejects_target() {
    assert!(!should_include("target"));
}

#[test]
fn includes_accepts_src() {
    assert!(should_include("src"));
}

#[test]
fn includes_accepts_readme() {
    assert!(should_include("README.md"));
}

#[test]
fn includes_accepts_dotfile_other_than_git() {
    assert!(should_include(".gitignore"));
    assert!(should_include(".env.example"));
}

// ── flatten ──────────────────────────────────────────────────────────────────

fn single_level_cache() -> (PathBuf, HashMap<PathBuf, DirCache>) {
    let root = PathBuf::from("/repo");
    let mut cache = HashMap::new();
    cache.insert(
        root.clone(),
        DirCache {
            children: vec![
                node("README.md", &root, false),
                node("crates", &root, true),
                node("docs", &root, true),
            ],
            loading: false,
            loaded: false,
        },
    );
    (root, cache)
}

#[test]
fn flatten_root_only_returns_immediate_children() {
    let (root, cache) = single_level_cache();
    let rows = flatten(&root, &cache, &HashSet::new());
    assert_eq!(rows.len(), 3);
}

#[test]
fn flatten_dirs_before_files() {
    let (root, cache) = single_level_cache();
    let rows = flatten(&root, &cache, &HashSet::new());
    assert!(rows[0].is_directory, "first row should be a dir");
    assert!(rows[1].is_directory, "second row should be a dir");
    assert!(!rows[2].is_directory, "third row should be a file");
}

#[test]
fn flatten_unexpanded_dir_hides_children() {
    let root = PathBuf::from("/repo");
    let src = root.join("src");
    let mut cache = HashMap::new();
    cache.insert(
        root.clone(),
        DirCache {
            children: vec![node("src", &root, true)],
            loading: false,
            loaded: false,
        },
    );
    cache.insert(
        src.clone(),
        DirCache {
            children: vec![node("main.rs", &src, false)],
            loading: false,
            loaded: false,
        },
    );
    let rows = flatten(&root, &cache, &HashSet::new());
    assert_eq!(
        rows.len(),
        1,
        "src children should be hidden when not expanded"
    );
}

#[test]
fn flatten_expanded_dir_shows_children_at_depth_1() {
    let root = PathBuf::from("/repo");
    let src = root.join("src");
    let mut cache = HashMap::new();
    cache.insert(
        root.clone(),
        DirCache {
            children: vec![node("src", &root, true), node("README.md", &root, false)],
            loading: false,
            loaded: false,
        },
    );
    cache.insert(
        src.clone(),
        DirCache {
            children: vec![node("lib.rs", &src, false)],
            loading: false,
            loaded: false,
        },
    );
    let mut expanded = HashSet::new();
    expanded.insert(src.clone());

    let rows = flatten(&root, &cache, &expanded);
    // src (depth 0) → lib.rs (depth 1) → README.md (depth 0)
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].name, "src");
    assert_eq!(rows[0].depth, 0);
    assert_eq!(rows[1].name, "lib.rs");
    assert_eq!(rows[1].depth, 1);
    assert_eq!(rows[2].name, "README.md");
    assert_eq!(rows[2].depth, 0);
}

#[test]
fn flatten_deep_nesting_depth_tags() {
    let root = PathBuf::from("/r");
    let a = root.join("a");
    let ab = a.join("b");
    let mut cache = HashMap::new();
    cache.insert(
        root.clone(),
        DirCache {
            children: vec![TreeNode {
                name: "a".into(),
                path: a.clone(),
                relative_path: PathBuf::from("a"),
                is_directory: true,
                depth: 0,
            }],
            loading: false,
            loaded: false,
        },
    );
    cache.insert(
        a.clone(),
        DirCache {
            children: vec![TreeNode {
                name: "b".into(),
                path: ab.clone(),
                relative_path: PathBuf::from("a/b"),
                is_directory: true,
                depth: 0,
            }],
            loading: false,
            loaded: false,
        },
    );
    cache.insert(
        ab.clone(),
        DirCache {
            children: vec![TreeNode {
                name: "leaf.rs".into(),
                path: ab.join("leaf.rs"),
                relative_path: PathBuf::from("a/b/leaf.rs"),
                is_directory: false,
                depth: 0,
            }],
            loading: false,
            loaded: false,
        },
    );
    let mut expanded = HashSet::new();
    expanded.insert(a.clone());
    expanded.insert(ab.clone());

    let rows = flatten(&root, &cache, &expanded);
    assert_eq!(rows[0].depth, 0); // a
    assert_eq!(rows[1].depth, 1); // b
    assert_eq!(rows[2].depth, 2); // leaf.rs
}

#[test]
fn flatten_case_insensitive_sort_within_group() {
    let root = PathBuf::from("/r");
    let mut cache = HashMap::new();
    cache.insert(
        root.clone(),
        DirCache {
            children: vec![
                node("Zoo.rs", &root, false),
                node("Alpha", &root, true),
                node("beta.rs", &root, false),
                node("Gamma", &root, true),
            ],
            loading: false,
            loaded: false,
        },
    );
    let rows = flatten(&root, &cache, &HashSet::new());
    // dirs: Alpha, Gamma; files: beta.rs, Zoo.rs
    assert_eq!(rows[0].name, "Alpha");
    assert_eq!(rows[1].name, "Gamma");
    assert_eq!(rows[2].name, "beta.rs");
    assert_eq!(rows[3].name, "Zoo.rs");
}
