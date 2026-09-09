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

pub mod include;
pub mod merge;
pub mod rename;
mod paths;
mod service;
pub mod setup;

pub use include::{CopyReport, Skip};
pub use merge::{
    MergePlan, MergeRefusal, MergeResult, apply_merge, merge_into_default, preflight_merge,
};
pub use rename::{
    RenameOutcome, RenamePlan, RenameRefusal, apply_rename, preflight_rename,
    rename_with_rollback,
};
pub use service::RepoWorktrees;
pub use setup::{SETUP_TIMEOUT, SetupOutcome, SetupTranscript};

use std::path::{Path, PathBuf};

use oximux_core::Workspace;
use oximux_git::Repository;
use oximux_settings::{ScriptKind, SetupDecision};
use oximux_storage::{StorageError, WorkspaceRepo};
use tokio::sync::mpsc::UnboundedSender;

/// One step of worktree provisioning, as it happens.
///
/// Provisioning is the slowest part of creating a worktree and the part most
/// likely to fail, so it is the part the user most needs to see. This is the
/// stream a host renders as a live transcript; a host with nowhere to show it
/// (`oximux serve`) simply passes no sink and the whole thing costs nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionEvent {
    /// An `.oximuxinclude` path landed in the worktree.
    IncludeCopied(PathBuf),
    /// An `.oximuxinclude` path did not, with the reason.
    IncludeSkipped(Skip),
    /// The setup script is about to run. Carries the script itself, because
    /// "which command produced this output" is the first question a failing
    /// transcript raises.
    SetupStarted(String),
    /// One line of merged stdout/stderr.
    SetupLine(String),
    /// The setup script ended.
    SetupFinished(SetupOutcome),
}

/// What provisioning should do for one `create` call.
///
/// [`Default`] is "inherit the project's answer, show nobody" — the shape every
/// existing call site had before provisioning existed, which is why adding this
/// parameter changed no behavior anywhere it was not deliberately wired up.
#[derive(Debug, Default)]
pub struct Provision {
    /// Per-request override for the setup script. [`SetupDecision::Inherit`]
    /// (the default) defers to the project's committed `auto_setup`.
    pub setup: SetupDecision,
    /// Where to stream [`ProvisionEvent`]s, if anyone is watching.
    pub sink: Option<UnboundedSender<ProvisionEvent>>,
    /// Whether a directory already at the target path may be deleted as debris
    /// from an interrupted create. See [`Provision::reclaiming_orphans`].
    ///
    /// Off by default, and deliberately so: the default has to be the one that
    /// cannot destroy anything.
    pub reclaim_orphan: bool,
}

impl Provision {
    /// Provision with a per-request decision and a live transcript sink.
    pub fn new(setup: SetupDecision, sink: UnboundedSender<ProvisionEvent>) -> Self {
        Self {
            setup,
            sink: Some(sink),
            reclaim_orphan: false,
        }
    }

    /// Opt in to clearing a directory already sitting at the target path.
    ///
    /// **Only for callers that own the path scheme.** The caller asserts two
    /// things by calling this: the path is host-derived
    /// (`<data_dir>/projects/<id>/worktrees/<slug>`, see [`worktree_path`]), so
    /// nothing a person put there by hand can be at that location; and the
    /// caller has already looked for a workspace row naming it.
    ///
    /// The second half is re-checked here rather than trusted — see
    /// [`reclaim_orphan`] — but the first half cannot be, which is why this is
    /// opt-in rather than automatic. The chat-initiated worktree uses a sibling
    /// `oximux-wt-<slug>` directory beside the project root, exactly where a
    /// person's own worktree could live, and must never turn this on.
    pub fn reclaiming_orphans(mut self) -> Self {
        self.reclaim_orphan = true;
        self
    }

    fn emit(&self, event: ProvisionEvent) {
        if let Some(sink) = &self.sink {
            let _ = sink.send(event);
        }
    }
}

/// Outcome of a create flow. Distinguishes user-visible failures (which
/// require explicit handling at the call site) from the silent success
/// path. The dirty-rollback variant is reached only when the rollback
/// path itself errors — caller should escalate visibility (e.g. surface
/// a "manual cleanup required" hint).
///
/// **Deliberately not boxed.** `Created(Workspace)` is the largest variant, and
/// `Workspace` grows every time the domain does (V026's `comment` and `phase`
/// were what first tripped `clippy::large_enum_variant` here). Boxing it would
/// add a heap allocation to the success path and a deref at every call site to
/// avoid a few hundred bytes moved *once per user-initiated worktree create* —
/// an operation that has already shelled out to git. The move is free next to
/// what surrounds it.
#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "see the note above: one move per git shell-out")]
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
    /// The project's `setup` script failed, so the worktree was never
    /// registered. Carries the whole transcript rather than a message because
    /// the useful part is the script's own output — a generic "setup failed"
    /// sends the user back to a terminal to reproduce it by hand.
    ///
    /// `rollback_error` is `Some` only when the rollback itself also failed,
    /// matching [`Self::StorageFailedRollbackDirty`]'s clean/dirty split.
    SetupFailed {
        transcript: SetupTranscript,
        rollback_error: Option<String>,
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
///
/// # Preconditions
///
/// The caller MUST have established that no workspace row references
/// `worktree_path`. Given that, a directory already at `worktree_path` is
/// debris from an interrupted create and is cleared before the git step — see
/// [`reclaim_orphan`].
///
/// # Provisioning
///
/// Between the git step and the DB insert, [`Provision`] runs two things that
/// make the difference between a worktree that exists and one the user can work
/// in: the `.oximuxinclude` copy, then the project's `setup` script. The order
/// is fixed — setup scripts read the files the include brings.
///
/// The insert deliberately happens *after* both. A row written before setup
/// would have to be deleted when setup fails, which is a fourth rollback step
/// and a fourth way to leave the three artifacts disagreeing. Provisioning
/// after the row exists would be worse still: the sidebar would list a
/// half-built worktree. As written, a setup failure unwinds through the
/// rollback ladder that was already here for the storage case.
#[allow(
    clippy::too_many_arguments,
    reason = "Seven of the eight are irreducible inputs to one operation: where the repo is, \
              what the workspace is called, where it goes, and what to write it into. Bundling \
              them into a params struct moves the same fields behind a name that means nothing \
              more than the function's own — and every one of the five call sites would then \
              build a struct to immediately destructure it. The eighth, `provision`, is already \
              the grouped form of what would otherwise be three."
)]
pub async fn create_workspace_with_rollback(
    project_root: &Path,
    project_id: &str,
    name: &str,
    slug: &str,
    worktree_path: &Path,
    linked_issue: Option<&str>,
    workspace_repo: &WorkspaceRepo,
    provision: &Provision,
) -> CreateOutcome {
    let branch = format!("oximux/{slug}");
    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => return CreateOutcome::GitFailed(format!("open project repo: {err}")),
    };
    if provision.reclaim_orphan
        && let Some(err) = reclaim_orphan(&repo, worktree_path, &branch, workspace_repo).await
    {
        return CreateOutcome::GitFailed(err);
    }
    if let Err(err) = repo.add_worktree(worktree_path, slug).await {
        return CreateOutcome::GitFailed(format!("add_worktree: {err}"));
    }

    // Include copy: best-effort by contract. Every skip is reported and nothing
    // here fails creation — a missing `.env` is worth telling the user about,
    // and the setup script is what decides whether it was actually required.
    //
    // On a blocking thread because it is synchronous, recursive filesystem work
    // and the desktop calls this whole function from gpui's *foreground*
    // executor. A project whose `.oximuxinclude` names a large directory would
    // otherwise freeze the window for the length of the copy.
    let copied = {
        let (root, wt) = (project_root.to_path_buf(), worktree_path.to_path_buf());
        match tokio::task::spawn_blocking(move || include::copy_included_files(&root, &wt)).await {
            Ok(report) => report,
            Err(err) => {
                // The blocking pool panicked or was shut down. The include copy
                // never fails creation, so neither does losing it — but it is
                // not something to pass over in silence either.
                tracing::warn!(?err, "oximuxinclude copy did not run");
                include::CopyReport::default()
            }
        }
    };
    for path in &copied.copied {
        provision.emit(ProvisionEvent::IncludeCopied(path.clone()));
    }
    for skip in &copied.skipped {
        tracing::info!(worktree = %worktree_path.display(), skip = %skip, "oximuxinclude skip");
        provision.emit(ProvisionEvent::IncludeSkipped(skip.clone()));
    }

    // Setup: reads `.oximux/scripts.toml` from the worktree, the same source
    // `run_cleanup_before_remove` uses — it is committed, so the branch's own
    // copy is the one that will actually run.
    let scripts = oximux_settings::load_for_project(worktree_path);
    if provision.setup.resolve(scripts.auto_setup)
        && let Some(script) = scripts.script(ScriptKind::Setup)
    {
        provision.emit(ProvisionEvent::SetupStarted(script.to_string()));
        let transcript =
            setup::run_setup_bounded(worktree_path, script, SETUP_TIMEOUT, provision.sink.as_ref())
                .await;
        provision.emit(ProvisionEvent::SetupFinished(transcript.outcome.clone()));
        if !transcript.outcome.is_ok() {
            tracing::warn!(
                worktree = %worktree_path.display(),
                outcome = %transcript.outcome.summary(),
                "setup failed during provisioning; rolling back"
            );
            let rollback_error = rollback(&repo, worktree_path, &branch).await;
            return CreateOutcome::SetupFailed {
                transcript,
                rollback_error,
            };
        }
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
        Err(insert_error) => match rollback(&repo, worktree_path, &branch).await {
            Some(rollback_error) => CreateOutcome::StorageFailedRollbackDirty {
                insert_error,
                rollback_error,
            },
            None => CreateOutcome::StorageFailedRollbackClean(insert_error),
        },
    }
}

/// Clear a worktree directory left behind by an interrupted create, so a retry
/// can proceed. Returns an error message when the path is occupied and could
/// not be cleared; `None` means the path is free.
///
/// **Why this exists.** Provisioning made the gap between `git worktree add`
/// and the workspace row up to [`SETUP_TIMEOUT`] wide. Kill the process during
/// a ten-minute install and the directory and branch survive with no row
/// naming them — invisible to the sidebar, and enough to make `add_worktree`
/// fail with "already exists" on every retry from then on. Before this, that
/// window was a few milliseconds and the case was theoretical.
///
/// **Why deleting is safe here — and the two things that make it so.** Reached
/// only when the caller opted in via [`Provision::reclaiming_orphans`], which
/// asserts the path is host-derived and therefore not somewhere a person keeps
/// work. On top of that, this re-checks that no workspace row names the path.
///
/// The row check is not redundant with the caller's own. A precondition that
/// lives only in a doc comment is one refactor away from being false, and the
/// cost of it being false here is a force-removed worktree with the user's
/// uncommitted work in it. This is a delete; it verifies rather than trusts.
async fn reclaim_orphan(
    repo: &Repository,
    worktree_path: &Path,
    branch: &str,
    workspace_repo: &WorkspaceRepo,
) -> Option<String> {
    if !worktree_path.exists() {
        return None;
    }
    // A row naming this path means it is a live workspace, not debris —
    // whatever the caller believed. Leave it alone and let `add_worktree`
    // produce its ordinary "already exists" error.
    match workspace_repo.get_by_worktree_path(&worktree_path.to_string_lossy()) {
        Ok(Some(existing)) => {
            tracing::warn!(
                worktree = %worktree_path.display(),
                workspace_id = %existing.id,
                "refusing to reclaim: a workspace row names this path"
            );
            return None;
        }
        Ok(None) => {}
        Err(err) => {
            // Could not prove it is unclaimed, so do not delete it.
            tracing::warn!(?err, "refusing to reclaim: workspace lookup failed");
            return None;
        }
    }
    tracing::warn!(
        worktree = %worktree_path.display(),
        branch,
        "worktree path occupied with no workspace row (interrupted create?); reclaiming"
    );
    // The same ladder the failure paths use: git knows about this worktree, so
    // let git detach it and drop the branch rather than tearing the directory
    // out from under `.git/worktrees`.
    let rollback_err = rollback(repo, worktree_path, branch).await;
    // Git can decline a path it never registered (a create killed between
    // `mkdir` and `worktree add`). The directory still has to go.
    if worktree_path.exists()
        && let Err(err) = std::fs::remove_dir_all(worktree_path)
    {
        return Some(format!(
            "worktree path {} is occupied and could not be cleared: {err}",
            worktree_path.display()
        ));
    }
    if let Some(err) = rollback_err {
        tracing::info!(%err, "orphan reclaim: git step complained but the path is clear");
    }
    None
}

/// Undo the git half of a create: force-remove the worktree, force-delete the
/// branch. Best-effort — both steps are attempted even when the first fails, so
/// a broken worktree does not also leave the branch behind. Returns the chained
/// error when either step failed, which is what makes an outcome "dirty".
///
/// Shared by the storage-failure and setup-failure arms: two rollback ladders
/// for the same two artifacts would be two chances to drift.
async fn rollback(repo: &Repository, worktree_path: &Path, branch: &str) -> Option<String> {
    let mut err = None;
    if let Err(e) = repo.remove_worktree(worktree_path, true).await {
        err = Some(format!("remove_worktree: {e}"));
    }
    if let Err(e) = repo.delete_branch(branch, true).await {
        err = Some(match err {
            Some(prev) => format!("{prev}; delete_branch: {e}"),
            None => format!("delete_branch: {e}"),
        });
    }
    err
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
