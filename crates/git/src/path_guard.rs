//! Containment for repository-relative paths that originate outside the desktop.
//!
//! Local callers hand these functions paths that came from `git status`, so they
//! are trustworthy. A remote-control client is not: without containment, a request
//! for `../../../.ssh/id_ed25519` (or an absolute path anywhere on disk) would read
//! that file straight off the developer's machine. Every filesystem/git RPC must
//! funnel its path argument through [`contained_path`] before touching disk.

use std::path::{Component, Path, PathBuf};

use crate::error::{GitError, Result};

/// Resolve `path` against `workdir` and guarantee the result stays inside it.
///
/// An **absolute path that lands inside the repository is allowed** — the desktop
/// diff view legitimately passes absolute paths. The invariant enforced here is
/// containment, not relativeness, so traversal is blocked without breaking that.
///
/// Resolution is canonical wherever possible: `canonicalize` resolves symlinks, so
/// a link *inside* the repo cannot be used to step outside it. A path that does not
/// exist yet cannot be canonicalized, so it falls back to a purely lexical fold of
/// `.`/`..` — a missing file is still checked rather than waved through, and the
/// caller's own read reports that it is missing.
pub fn contained_path(workdir: &Path, path: &Path) -> Result<PathBuf> {
    // Canonicalize the root too, so both sides of the prefix check are in the same
    // form (on macOS `/var/...` is a symlink to `/private/var/...`, and comparing
    // one against the other would reject legitimate paths).
    let root = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let joined = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    let resolved = joined.canonicalize().unwrap_or_else(|_| normalize_lexically(&joined));

    if !resolved.starts_with(&root) {
        return Err(GitError::parse(format!(
            "path escapes the repository: {}",
            path.display()
        )));
    }
    Ok(resolved)
}

/// Fold `.` and `..` away without touching the filesystem, for paths that do not
/// exist. `..` pops the previous component; popping past the root is a no-op, so
/// `/repo/../../etc` collapses to `/etc` and then fails the prefix check.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::contained_path;
    use std::path::Path;

    /// A repo dir with one real file, plus the canonical root for comparisons.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ok.txt"), b"x\n").expect("write");
        let root = dir.path().canonicalize().expect("canonicalize root");
        (dir, root)
    }

    #[test]
    fn relative_path_inside_the_repo_is_allowed() {
        let (dir, root) = repo();
        let got = contained_path(dir.path(), Path::new("ok.txt")).expect("allowed");
        assert_eq!(got, root.join("ok.txt"));
    }

    /// The desktop diff view passes absolute paths; containment, not relativeness,
    /// is the rule.
    #[test]
    fn absolute_path_inside_the_repo_is_allowed() {
        let (dir, root) = repo();
        let abs = dir.path().join("ok.txt");
        let got = contained_path(dir.path(), &abs).expect("allowed");
        assert_eq!(got, root.join("ok.txt"));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let (dir, _root) = repo();
        for attack in ["../secret", "../../../.ssh/id_ed25519", "sub/../../outside"] {
            assert!(
                contained_path(dir.path(), Path::new(attack)).is_err(),
                "{attack} must not escape",
            );
        }
    }

    /// Traversal is rejected even when the target does not exist, so the check
    /// never depends on `canonicalize` succeeding.
    #[test]
    fn traversal_to_a_missing_file_is_still_rejected() {
        let (dir, _root) = repo();
        let r = contained_path(dir.path(), Path::new("../definitely/not/here.txt"));
        assert!(r.is_err(), "a missing path outside the repo is still an escape");
    }

    #[test]
    fn absolute_path_outside_the_repo_is_rejected() {
        let (dir, _root) = repo();
        assert!(contained_path(dir.path(), Path::new("/etc/passwd")).is_err());
    }

    /// `..` that stays inside is fine — the rule is where you land, not how.
    #[test]
    fn traversal_that_lands_back_inside_is_allowed() {
        let (dir, root) = repo();
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let got = contained_path(dir.path(), Path::new("sub/../ok.txt")).expect("allowed");
        assert_eq!(got, root.join("ok.txt"));
    }

    /// A symlink inside the repo must not be a way out — this is why the check
    /// canonicalizes rather than only folding lexically.
    #[cfg(unix)]
    #[test]
    fn symlink_pointing_outside_the_repo_is_rejected() {
        let (dir, _root) = repo();
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("secret.txt"), b"s\n").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link"))
            .expect("symlink");

        assert!(
            contained_path(dir.path(), Path::new("link")).is_err(),
            "a symlink inside the repo must not read outside it",
        );
    }
}
