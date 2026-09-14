//! External worktree discovery: the set difference between what git knows
//! and what the database tracks.
//!
//! OxiMux's rail renders only rows from its own `workspaces` table. A worktree
//! made by `git worktree add` in a terminal, or by another tool, is invisible
//! even though it sits in the same repository. Git keeps the registry of every
//! linked worktree in the main repository (`.git/worktrees/`), so one
//! `git worktree list` per project sees all of them wherever they live; this
//! module subtracts the rows the database already has and hands the rest to
//! the rail as "untracked".
//!
//! Pure: the scan is run and cached by the worktree-stats refresher; nothing
//! here touches git or the database.

use std::path::{Path, PathBuf};

use oximux_core::WorktreeInfo;

/// A worktree git lists for a project that no `workspaces` row points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedWorktree {
    pub project_id: String,
    /// The path as git printed it — the spelling the user made it with.
    pub path: PathBuf,
    /// Checked-out branch; `None` when HEAD is detached.
    pub branch: Option<String>,
}

impl UntrackedWorktree {
    /// The directory's own name — the row title before adoption gives it one.
    pub fn dir_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// One path in the shape every comparison here uses: the real directory when
/// it exists, else its components (so a trailing slash or a `.` segment does
/// not make two spellings of a gone directory look different; a `..` segment
/// is kept as written, since resolving it without the filesystem would guess).
///
/// `canonicalize` is the precedent `Repository::worktree_at` set: git's
/// `--porcelain` output is not `fs::canonicalize`'s shape (on Windows git
/// prints `C:/…`, canonicalize returns `\\?\C:\…`), and the database holds
/// whatever spelling the row was created with. Resolving BOTH sides is what
/// makes the comparison mean "same directory" rather than "same spelling".
fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.components().collect())
}

/// The worktrees in `on_disk` (one project's `git worktree list`) that no
/// tracked row points at.
///
/// Excluded, in order: the main worktree (the project itself, whose row is
/// synthesized at render time); anything at the project root by another
/// spelling; every path in `tracked_paths` — **active and archived rows
/// alike**, or an archived worktree would resurface as untracked the moment
/// it was archived; and a listing whose directory no longer exists — git keeps
/// a deleted worktree in its registry as prunable, and a row for a directory
/// that is not there is not something to adopt.
pub fn reconcile(
    project_id: &str,
    project_root: &Path,
    on_disk: Vec<WorktreeInfo>,
    tracked_paths: &[String],
) -> Vec<UntrackedWorktree> {
    let root = resolve(project_root);
    let tracked: Vec<PathBuf> = tracked_paths
        .iter()
        .map(|p| resolve(Path::new(p)))
        .collect();
    on_disk
        .into_iter()
        .filter(|w| !w.is_main && w.path.is_dir())
        .filter(|w| {
            let here = resolve(&w.path);
            here != root && !tracked.contains(&here)
        })
        .map(|w| UntrackedWorktree {
            project_id: project_id.to_string(),
            path: w.path,
            branch: w.branch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(path: &Path, branch: Option<&str>, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: path.to_path_buf(),
            branch: branch.map(str::to_string),
            head: "deadbeef".into(),
            is_main,
            is_locked: false,
        }
    }

    /// A repo root plus three linked worktree directories, all real.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let a = tmp.path().join("wt").join("a");
        let b = tmp.path().join("wt").join("b");
        let c = tmp.path().join("elsewhere").join("c");
        for d in [&root, &a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        (tmp, root, a, b, c)
    }

    #[test]
    fn the_main_worktree_and_tracked_rows_are_never_untracked() {
        let (_tmp, root, a, b, c) = fixture();
        let on_disk = vec![
            info(&root, Some("main"), true),
            info(&a, Some("feat-a"), false),
            info(&b, Some("feat-b"), false),
            info(&c, None, false),
        ];
        let tracked = vec![a.to_string_lossy().into_owned()];
        let got = reconcile("p", &root, on_disk, &tracked);
        let paths: Vec<&Path> = got.iter().map(|u| u.path.as_path()).collect();
        assert_eq!(paths, vec![b.as_path(), c.as_path()]);
        assert_eq!(got[0].branch.as_deref(), Some("feat-b"));
        assert_eq!(got[1].branch, None, "a detached checkout is still a worktree");
        assert_eq!(got[1].dir_name(), "c");
    }

    /// An archived row still points at its directory; it must count as
    /// tracked or `Archive` would immediately resurface the worktree.
    #[test]
    fn an_archived_row_counts_as_tracked() {
        let (_tmp, root, a, _b, _c) = fixture();
        let on_disk = vec![info(&root, Some("main"), true), info(&a, Some("old"), false)];
        // The caller passes active + archived paths together.
        let tracked = vec![a.to_string_lossy().into_owned()];
        assert!(reconcile("p", &root, on_disk, &tracked).is_empty());
    }

    #[test]
    fn a_trailing_slash_and_a_dot_segment_still_match() {
        let (_tmp, root, a, _b, _c) = fixture();
        let on_disk = vec![info(&root, Some("main"), true), info(&a, Some("x"), false)];
        let with_slash = format!("{}{}", a.display(), std::path::MAIN_SEPARATOR);
        let with_dot = a.join(".").display().to_string();
        assert!(reconcile("p", &root, on_disk.clone(), &[with_slash]).is_empty());
        assert!(reconcile("p", &root, on_disk, &[with_dot]).is_empty());
    }

    /// A row that reaches the directory through a symlink is the same row.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_still_matches() {
        let (tmp, root, a, _b, _c) = fixture();
        let link = tmp.path().join("link-to-a");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let on_disk = vec![info(&root, Some("main"), true), info(&a, Some("x"), false)];
        assert!(reconcile("p", &root, on_disk, &[link.to_string_lossy().into_owned()]).is_empty());
    }

    /// Git lists a deleted worktree as prunable; there is nothing to adopt.
    #[test]
    fn a_listing_whose_directory_is_gone_is_dropped() {
        let (_tmp, root, a, _b, _c) = fixture();
        std::fs::remove_dir_all(&a).unwrap();
        let on_disk = vec![info(&root, Some("main"), true), info(&a, Some("x"), false)];
        assert!(reconcile("p", &root, on_disk, &[]).is_empty());
    }

    /// A tracked row whose directory is gone must not poison the comparison
    /// for the rows that are still there.
    #[test]
    fn a_tracked_row_with_a_gone_directory_is_harmless() {
        let (_tmp, root, a, b, _c) = fixture();
        std::fs::remove_dir_all(&b).unwrap();
        let on_disk = vec![info(&root, Some("main"), true), info(&a, Some("x"), false)];
        let tracked = vec![b.to_string_lossy().into_owned()];
        let got = reconcile("p", &root, on_disk, &tracked);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, a);
    }

    #[test]
    fn the_project_root_under_another_spelling_is_the_project() {
        let (_tmp, root, _a, _b, _c) = fixture();
        let spelled = root.join(".");
        let on_disk = vec![info(&spelled, Some("main"), false)];
        assert!(reconcile("p", &root, on_disk, &[]).is_empty());
    }
}
