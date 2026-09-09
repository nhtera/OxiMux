//! Path comparisons shared by the worktree mutations.
//!
//! Both live in one place because both are answering questions about *live
//! process directories*, and both have a specific wrong answer that has already
//! cost this crate a defect: a literal `==` that calls `/tmp/x` and
//! `/private/tmp/x` different directories on macOS, and a string prefix that
//! calls `/wt/feat` the parent of `/wt/feature`.

use std::path::Path;

/// Whether two paths name the same directory, resolving symlinks when both
/// exist.
///
/// A plain `==` would call `/tmp/wt` and `/private/tmp/wt` different directories
/// on macOS, where `/tmp` is a symlink — and the holder set is built from live
/// process cwds, which are already resolved. Falling back to a literal compare
/// keeps this usable for a destination that does not exist yet.
pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Whether `path` is `root` or sits anywhere beneath it.
///
/// Component-wise via [`Path::starts_with`], never string prefixes: `/wt/feat`
/// is a string prefix of `/wt/feature` but not a parent of it, and treating it
/// as one would refuse renames on a sibling worktree forever.
pub(crate) fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_equal_matches_a_path_against_itself() {
        let dir = tempfile::tempdir().unwrap();
        assert!(paths_equal(dir.path(), dir.path()));
        assert!(!paths_equal(dir.path(), &dir.path().join("child")));
    }

    #[test]
    fn paths_equal_falls_back_to_a_literal_compare_for_missing_paths() {
        // The destination of a rename does not exist yet, so canonicalize fails
        // on it and the literal compare has to carry the answer.
        assert!(paths_equal(
            Path::new("/definitely/not/here"),
            Path::new("/definitely/not/here")
        ));
        assert!(!paths_equal(
            Path::new("/definitely/not/here"),
            Path::new("/somewhere/else")
        ));
    }
}
