//! The worktree lifecycle, shared by every host.
//!
//! A worktree is three things that must agree: a git worktree on disk, the
//! `oximux/<slug>` branch it checks out, and the `workspaces` row that names
//! both. [`create_workspace_with_rollback`] is what keeps them in agreement
//! when the third step fails after the first two succeeded, and it is the
//! reason this module exists as one implementation rather than two.
//!
//! The path scheme is host-derived on purpose: a client names a project and a
//! slug, never a location. [`worktree_path`] is that derivation, taking the
//! data directory explicitly so `oximux serve --data-dir` puts worktrees under
//! its own root instead of the desktop's.

mod service;

pub use service::RepoWorktrees;

use std::path::{Path, PathBuf};

use oximux_core::Workspace;
use oximux_git::Repository;
use oximux_settings::ScriptKind;
use oximux_storage::{StorageError, WorkspaceRepo};

/// Outcome of a create flow. Distinguishes user-visible failures (which
/// require explicit handling at the call site) from the silent success
/// path. The dirty-rollback variant is reached only when the rollback
/// path itself errors — caller should escalate visibility (e.g. surface
/// a "manual cleanup required" hint).
#[derive(Debug)]
pub enum CreateOutcome {
    /// Workspace row inserted; worktree + branch live on disk.
    Created(Workspace),
    /// The git step failed before any rollback was needed. The repo is
    /// in a clean state.
    GitFailed(String),
    /// Storage insert failed and the rollback (`remove_worktree` +
    /// `delete_branch`) ran cleanly — repo + DB consistent again, but
    /// the user's request failed.
    StorageFailedRollbackClean(StorageError),
    /// Storage insert failed AND the rollback itself failed. The repo
    /// has an orphan worktree or branch; surface the original error and
    /// the rollback error.
    StorageFailedRollbackDirty {
        insert_error: StorageError,
        rollback_error: String,
    },
}

/// Compose the worktree dir path:
/// `<data_dir>/projects/<project_id>/worktrees/<slug>`.
///
/// `data_dir` is passed rather than resolved here because the two hosts
/// disagree about it: the desktop always uses its own app data root, while
/// `oximux serve` honours `--data-dir`. Deriving it internally would put a
/// server's worktrees under the desktop's directory.
pub fn worktree_path(data_dir: &Path, project_id: &str, slug: &str) -> PathBuf {
    data_dir
        .join("projects")
        .join(project_id)
        .join("worktrees")
        .join(slug)
}

/// Open the project repo, create a new worktree on branch `oximux/<slug>`,
/// and insert the workspace row. On storage failure, runs the rollback
/// (force-remove worktree + force-delete branch) so that the next listing
/// reflects the on-disk truth.
///
/// `name` is the human label (caller has already trimmed); `slug` MUST
/// pre-validate via `validate_slug` upstream — this function assumes the
/// slug is safe to pass to `git worktree add -b oximux/<slug>`.
pub async fn create_workspace_with_rollback(
    project_root: &Path,
    project_id: &str,
    name: &str,
    slug: &str,
    worktree_path: &Path,
    linked_issue: Option<&str>,
    workspace_repo: &WorkspaceRepo,
) -> CreateOutcome {
    let branch = format!("oximux/{slug}");
    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => return CreateOutcome::GitFailed(format!("open project repo: {err}")),
    };
    if let Err(err) = repo.add_worktree(worktree_path, slug).await {
        return CreateOutcome::GitFailed(format!("add_worktree: {err}"));
    }
    let path_str = worktree_path.to_string_lossy().to_string();
    match workspace_repo.insert(project_id, name, slug, &branch, &path_str) {
        Ok(mut workspace) => {
            // Best-effort metadata write — the worktree + row already exist, so
            // a failure here only loses the issue badge, not the workspace. The
            // in-memory field is set ONLY on a confirmed write.
            if let Some(issue) = linked_issue {
                match workspace_repo.set_linked_issue(&workspace.id, Some(issue)) {
                    Ok(()) => workspace.linked_issue = Some(issue.to_string()),
                    Err(err) => {
                        tracing::warn!(?err, workspace_id = %workspace.id, "set_linked_issue failed")
                    }
                }
            }
            CreateOutcome::Created(workspace)
        }
        Err(insert_error) => {
            // Rollback: best-effort. If either step errors, we surface
            // a dirty-state outcome so the call site can log loudly.
            let mut rollback_err = None;
            if let Err(err) = repo.remove_worktree(worktree_path, true).await {
                rollback_err = Some(format!("remove_worktree: {err}"));
            }
            if let Err(err) = repo.delete_branch(&branch, true).await {
                let chained = match rollback_err {
                    Some(prev) => format!("{prev}; delete_branch: {err}"),
                    None => format!("delete_branch: {err}"),
                };
                rollback_err = Some(chained);
            }
            match rollback_err {
                Some(err) => CreateOutcome::StorageFailedRollbackDirty {
                    insert_error,
                    rollback_error: err,
                },
                None => CreateOutcome::StorageFailedRollbackClean(insert_error),
            }
        }
    }
}

/// Max time to wait for a per-project `cleanup` teardown before forcing the
/// worktree removal anyway.
const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the project's `cleanup` script (from `.oximux/scripts.toml`) to
/// completion at `worktree_path` BEFORE the worktree is removed, bounded by
/// [`CLEANUP_TIMEOUT`]. Best-effort and non-blocking to deletion: a missing
/// script, a non-zero exit, an exec failure, or a timeout are each logged and
/// then ignored — teardown must never trap the user behind a failed remove.
/// `kill_on_drop` ensures a hung child is killed when the timeout future is
/// dropped (the force-remove escape). Output is discarded; this is a captured
/// subprocess, distinct from the interactive "Run cleanup" terminal tab.
pub async fn run_cleanup_before_remove(worktree_path: &Path) {
    run_cleanup_bounded(worktree_path, CLEANUP_TIMEOUT).await;
}

/// Inner implementation with an injectable timeout so the force-escape (a hung
/// cleanup must not block removal) can be unit-tested with a short bound.
async fn run_cleanup_bounded(worktree_path: &Path, timeout: std::time::Duration) {
    let scripts = oximux_settings::load_for_project(worktree_path);
    let Some(cleanup) = scripts.script(ScriptKind::Cleanup) else {
        return;
    };
    let cleanup = cleanup.to_string();
    let mut cmd = tokio::process::Command::new("sh");
    {
        use oximux_no_window::NoWindow as _;
        cmd.arg("-lc")
            .arg(&cleanup)
            .current_dir(worktree_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .no_window()
            .kill_on_drop(true);
    }
    let wt = worktree_path.display();
    match tokio::time::timeout(timeout, cmd.status()).await {
        Ok(Ok(status)) if status.success() => {
            tracing::info!(worktree = %wt, "cleanup script completed before removal");
        }
        Ok(Ok(status)) => {
            tracing::warn!(worktree = %wt, ?status, "cleanup script exited non-zero; removing anyway");
        }
        Ok(Err(err)) => {
            tracing::warn!(worktree = %wt, ?err, "cleanup script failed to start; removing anyway");
        }
        Err(_) => {
            tracing::warn!(worktree = %wt, "cleanup script timed out; killed, removing anyway");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_is_host_derived_from_the_data_dir() {
        let path = worktree_path(Path::new("/data"), "proj-1", "feat-x");
        assert_eq!(path, Path::new("/data/projects/proj-1/worktrees/feat-x"));
    }

    /// The whole reason `data_dir` is a parameter: two hosts, two roots.
    #[test]
    fn a_different_data_dir_relocates_the_worktree() {
        let serve = worktree_path(Path::new("/srv/oximux"), "proj-1", "feat-x");
        let desktop = worktree_path(Path::new("/home/u/Library"), "proj-1", "feat-x");
        assert_ne!(serve, desktop);
        assert!(serve.starts_with("/srv/oximux"));
    }

    use std::time::{Duration, Instant};

    fn write_cleanup(dir: &Path, body: &str) {
        let oximux = dir.join(".oximux");
        std::fs::create_dir_all(&oximux).unwrap();
        std::fs::write(oximux.join("scripts.toml"), format!("cleanup = {body:?}\n")).unwrap();
    }

    // The force-escape: a hung cleanup must not block beyond the timeout.
    #[tokio::test]
    async fn hung_cleanup_is_bounded_by_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        write_cleanup(tmp.path(), "sleep 60");
        let start = Instant::now();
        run_cleanup_bounded(tmp.path(), Duration::from_millis(200)).await;
        // Without the timeout this would block ~60s; the bound + kill_on_drop
        // must return it well under that.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "cleanup should be killed at the timeout, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn no_cleanup_script_returns_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        // No .oximux/scripts.toml → no-op, no panic, near-instant.
        let start = Instant::now();
        run_cleanup_bounded(tmp.path(), Duration::from_secs(30)).await;
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn fast_cleanup_completes_normally() {
        let tmp = tempfile::tempdir().unwrap();
        write_cleanup(tmp.path(), "true");
        // Should complete (success arm) well within the timeout.
        run_cleanup_bounded(tmp.path(), Duration::from_secs(10)).await;
    }
}
