//! Flat-row tree model for the file explorer.
//!
//! Pure module — no GPUI imports. `flatten` builds a depth-tagged row list
//! from the lazy-loaded directory cache; `should_include` gates which
//! filesystem entries are visible.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A single visible row in the flat explorer list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub name: String,
    /// Absolute filesystem path.
    pub path: PathBuf,
    /// Path relative to the repo root.
    pub relative_path: PathBuf,
    pub is_directory: bool,
    /// Depth from root (0 = root's immediate children).
    pub depth: usize,
}

/// Cached children for one directory.
#[derive(Debug, Default, Clone)]
pub struct DirCache {
    pub children: Vec<TreeNode>,
    pub loading: bool,
    /// Set to `true` once the directory has been fully loaded at least once.
    /// Distinguishes "empty directory" from "never loaded" so we don't loop.
    pub loaded: bool,
}

/// Returns `true` if the entry should appear in the explorer.
/// Rejects sentinel dirs that bloat and confuse the view.
pub fn should_include(name: &str) -> bool {
    !matches!(name, ".git" | "node_modules" | "target")
}

/// Build the flat visible-row list from the cache + expanded set.
///
/// Walk is breadth-at-depth — for each level, emit dirs first (alphabetically,
/// case-insensitive), then files (alphabetically, case-insensitive). For any
/// directory in the `expanded` set, recursively emit its children at depth+1.
pub fn flatten(
    root: &Path,
    cache: &HashMap<PathBuf, DirCache>,
    expanded: &HashSet<PathBuf>,
) -> Vec<TreeNode> {
    let mut out = Vec::new();
    if let Some(dir) = cache.get(root) {
        flatten_children(&dir.children, cache, expanded, 0, &mut out);
    }
    out
}

/// Drop rows whose relative path equals or descends from any path in
/// `ignored`. Pass-through when `show_ignored` is true.
///
/// `starts_with` is component-wise on `Path`, so `"target"` matches
/// `"target/release/foo"` but not `"target_test/foo"` — the safe behavior
/// for "is this row beneath an ignored directory?". Pure function: extracted
/// from `FileExplorer::recompute_rows` so the filter logic can be unit-tested
/// without spinning up the GPUI entity.
pub fn filter_visible(
    rows: Vec<TreeNode>,
    ignored: &[PathBuf],
    show_ignored: bool,
) -> Vec<TreeNode> {
    if show_ignored {
        return rows;
    }
    rows.into_iter()
        .filter(|n| {
            !ignored
                .iter()
                .any(|i| n.relative_path == *i || n.relative_path.starts_with(i))
        })
        .collect()
}

fn flatten_children(
    children: &[TreeNode],
    cache: &HashMap<PathBuf, DirCache>,
    expanded: &HashSet<PathBuf>,
    depth: usize,
    out: &mut Vec<TreeNode>,
) {
    // Sort: dirs first, then files; ties broken case-insensitively by name.
    // This mirrors the sort applied by `fs_load` but works even when the cache
    // was built from test fixtures inserted in arbitrary order.
    let mut sorted: Vec<&TreeNode> = children.iter().collect();
    sorted.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    for child in sorted {
        let mut row = child.clone();
        row.depth = depth;
        let is_dir = row.is_directory;
        let path = row.path.clone();
        out.push(row);
        if is_dir
            && expanded.contains(&path)
            && let Some(sub) = cache.get(&path)
        {
            flatten_children(&sub.children, cache, expanded, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(
        name: &str,
        path: PathBuf,
        relative_path: PathBuf,
        is_directory: bool,
    ) -> TreeNode {
        TreeNode {
            name: name.to_string(),
            path,
            relative_path,
            is_directory,
            depth: 0,
        }
    }

    // ── should_include ──────────────────────────────────────────────────────

    #[test]
    fn should_include_rejects_dot_git() {
        assert!(!should_include(".git"));
    }

    #[test]
    fn should_include_rejects_node_modules() {
        assert!(!should_include("node_modules"));
    }

    #[test]
    fn should_include_rejects_target() {
        assert!(!should_include("target"));
    }

    #[test]
    fn should_include_accepts_normal_names() {
        assert!(should_include("src"));
        assert!(should_include("README.md"));
        assert!(should_include("Cargo.toml"));
        assert!(should_include(".gitignore")); // dotfiles that are not .git are fine
    }

    // ── flatten ─────────────────────────────────────────────────────────────

    fn root_only_cache() -> (PathBuf, HashMap<PathBuf, DirCache>) {
        let root = PathBuf::from("/repo");
        let mut cache = HashMap::new();
        let children = vec![
            make_node(
                "zfile.rs",
                root.join("zfile.rs"),
                PathBuf::from("zfile.rs"),
                false,
            ),
            make_node("crates", root.join("crates"), PathBuf::from("crates"), true),
            make_node(
                "README.md",
                root.join("README.md"),
                PathBuf::from("README.md"),
                false,
            ),
        ];
        cache.insert(
            root.clone(),
            DirCache {
                children,
                loading: false,
                loaded: false,
            },
        );
        (root, cache)
    }

    #[test]
    fn flatten_root_only_no_expanded() {
        let (root, cache) = root_only_cache();
        let rows = flatten(&root, &cache, &HashSet::new());
        // dirs first then files, alphabetically within each group
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "crates");
        assert!(rows[0].is_directory);
        assert_eq!(rows[1].name, "README.md");
        assert_eq!(rows[2].name, "zfile.rs");
    }

    #[test]
    fn flatten_expanded_dir_shows_children() {
        let root = PathBuf::from("/repo");
        let mut cache = HashMap::new();
        let root_children = vec![
            make_node("src", root.join("src"), PathBuf::from("src"), true),
            make_node(
                "main.rs",
                root.join("main.rs"),
                PathBuf::from("main.rs"),
                false,
            ),
        ];
        cache.insert(
            root.clone(),
            DirCache {
                children: root_children,
                loading: false,
                loaded: false,
            },
        );
        let src = root.join("src");
        let src_children = vec![make_node(
            "lib.rs",
            src.join("lib.rs"),
            PathBuf::from("src/lib.rs"),
            false,
        )];
        cache.insert(
            src.clone(),
            DirCache {
                children: src_children,
                loading: false,
                loaded: false,
            },
        );

        let mut expanded = HashSet::new();
        expanded.insert(src.clone());

        let rows = flatten(&root, &cache, &expanded);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "src");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].name, "lib.rs");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].name, "main.rs");
        assert_eq!(rows[2].depth, 0);
    }

    #[test]
    fn flatten_deep_nesting() {
        let root = PathBuf::from("/r");
        let a = root.join("a");
        let ab = a.join("b");
        let mut cache = HashMap::new();
        cache.insert(
            root.clone(),
            DirCache {
                children: vec![make_node("a", a.clone(), PathBuf::from("a"), true)],
                loading: false,
                loaded: false,
            },
        );
        cache.insert(
            a.clone(),
            DirCache {
                children: vec![make_node("b", ab.clone(), PathBuf::from("a/b"), true)],
                loading: false,
                loaded: false,
            },
        );
        cache.insert(
            ab.clone(),
            DirCache {
                children: vec![make_node(
                    "leaf.rs",
                    ab.join("leaf.rs"),
                    PathBuf::from("a/b/leaf.rs"),
                    false,
                )],
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

    // ── filter_visible ──────────────────────────────────────────────────────

    fn row(rel: &str, is_dir: bool) -> TreeNode {
        TreeNode {
            name: rel.split('/').next_back().unwrap_or(rel).to_string(),
            path: PathBuf::from(format!("/repo/{rel}")),
            relative_path: PathBuf::from(rel),
            is_directory: is_dir,
            depth: 0,
        }
    }

    #[test]
    fn filter_visible_passthrough_when_show_ignored_true() {
        let rows = vec![row("target", true), row("README.md", false)];
        let ignored = vec![PathBuf::from("target")];
        let out = filter_visible(rows.clone(), &ignored, true);
        assert_eq!(out, rows, "show_ignored=true must return rows unchanged");
    }

    #[test]
    fn filter_visible_drops_direct_ignored_entry() {
        let rows = vec![
            row(".DS_Store", false),
            row("README.md", false),
            row("plans", true),
        ];
        let ignored = vec![PathBuf::from(".DS_Store"), PathBuf::from("plans")];
        let out = filter_visible(rows, &ignored, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "README.md");
    }

    #[test]
    fn filter_visible_drops_descendants_of_ignored_dir() {
        // Simulates: user expanded `target/` before toggling hide-ignored on.
        // Git only emits `!` for `target` itself; descendants must still go.
        let rows = vec![
            row("src/main.rs", false),
            row("target", true),
            row("target/release", true),
            row("target/release/foo", false),
            row("target_test/keep_me.rs", false),
        ];
        let ignored = vec![PathBuf::from("target")];
        let out = filter_visible(rows, &ignored, false);
        let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"main.rs"),
            "non-ignored sibling must survive"
        );
        assert!(
            names.contains(&"keep_me.rs"),
            "`target_test/` is NOT a descendant of `target/` (component-wise prefix)"
        );
        assert!(!names.contains(&"target"), "direct ignored row dropped");
        assert!(
            !names.contains(&"release"),
            "ignored descendant dir dropped"
        );
        assert!(!names.contains(&"foo"), "ignored descendant file dropped");
    }

    #[test]
    fn flatten_sort_order_dirs_first_case_insensitive() {
        let root = PathBuf::from("/r");
        let mut cache = HashMap::new();
        let children = vec![
            make_node(
                "Zoo.rs",
                root.join("Zoo.rs"),
                PathBuf::from("Zoo.rs"),
                false,
            ),
            make_node("Beta", root.join("Beta"), PathBuf::from("Beta"), true),
            make_node(
                "alpha.rs",
                root.join("alpha.rs"),
                PathBuf::from("alpha.rs"),
                false,
            ),
            make_node(
                "alpha_dir",
                root.join("alpha_dir"),
                PathBuf::from("alpha_dir"),
                true,
            ),
        ];
        cache.insert(
            root.clone(),
            DirCache {
                children,
                loading: false,
                loaded: false,
            },
        );
        let rows = flatten(&root, &cache, &HashSet::new());
        // dirs first: Alpha_dir, Beta — then files: alpha.rs, Zoo.rs
        assert_eq!(rows[0].name, "alpha_dir");
        assert_eq!(rows[1].name, "Beta");
        assert_eq!(rows[2].name, "alpha.rs");
        assert_eq!(rows[3].name, "Zoo.rs");
    }
}
