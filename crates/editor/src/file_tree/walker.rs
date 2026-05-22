//! Gitignore-aware single-level directory enumerator.
//!
//! `WalkBuilder::max_depth(Some(1))` lists only the direct children of one
//! directory — no recursive subtree walk. The walker still composes the full
//! gitignore precedence chain (repo `.gitignore` + nested `.gitignore` +
//! `.git/info/exclude` + global) so the visible tree mirrors what `git
//! status` would consider tracked.
//!
//! `hidden(false)` keeps dotfiles visible but does NOT auto-exclude `.git/`
//! itself (an upstream wontfix); `filter_entry` handles that explicitly.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Directories always skipped at the walker level — never even appear in
/// the tree. Skipping at the walker (not just hiding in UI) keeps a
/// `node_modules/` storm from drowning the watcher debounce queue.
///
/// Shared with `watcher::is_ignored` via the parent module so the two
/// filters can never silently diverge (one extending the list while the
/// other doesn't would have files appearing in the tree but no refresh
/// events for them, or vice versa).
pub(crate) const ALWAYS_SKIP: &[&str] = crate::file_tree::SKIP_NAMES;

/// Direct children of `dir` (depth 1). Returns `(path, is_dir)` pairs in
/// filesystem order — caller must sort. Runs on the background executor;
/// `< 1ms` for typical project directories.
pub fn list_children(dir: &Path) -> Vec<(PathBuf, bool)> {
    WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !ALWAYS_SKIP.contains(&name.as_ref())
        })
        .build()
        .filter_map(|r| r.ok())
        .filter(|e| e.depth() > 0)
        .map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (e.into_path(), is_dir)
        })
        .collect()
}

/// Sort: directories first, then files; alphabetical case-insensitive.
/// `to_ascii_lowercase()` is fine — file names are ASCII-dominant and
/// non-ASCII still sorts deterministically by codepoint.
pub fn sort_entries(entries: &mut [(PathBuf, bool)]) {
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            let na =
                a.0.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase();
            let nb =
                b.0.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase();
            na.cmp(&nb)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sort_dirs_first_alpha() {
        let mut v: Vec<(PathBuf, bool)> = vec![
            (PathBuf::from("/r/zeta.rs"), false),
            (PathBuf::from("/r/Alpha"), true),
            (PathBuf::from("/r/main.rs"), false),
            (PathBuf::from("/r/beta"), true),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v
            .iter()
            .map(|(p, d)| (p.file_name().unwrap().to_string_lossy().to_string(), *d))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Alpha".to_string(), true),
                ("beta".to_string(), true),
                ("main.rs".to_string(), false),
                ("zeta.rs".to_string(), false),
            ]
        );
    }
}
