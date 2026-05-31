//! Fetch the AI commit-message generation context from a worktree.
//!
//! Bundles three shellouts the agent prompt needs:
//!
//! 1. `git rev-parse --abbrev-ref HEAD` — current branch (or `None` if detached)
//! 2. `git diff --cached --name-status` — short name+status summary (capped)
//! 3. `git diff --cached --patch --minimal --no-color --no-ext-diff` — the
//!    actual staged patch the agent reasons about (capped at the prompt-side
//!    budget; this fn returns whatever git produced and the prompt builder
//!    truncates further if needed)
//!
//! Returns `None` when the staged summary is empty — nothing staged means
//! nothing to generate from, and callers should refuse to spawn the
//! generator rather than send the agent an empty diff.
//!
//! All three shellouts use the existing [`GitCmd`] machinery — same
//! environment hardening (LANG=C, GIT_OPTIONAL_LOCKS=0,
//! GIT_CONFIG_NOSYSTEM=1, kill_on_drop), same timeout policy, same error
//! shape.

use std::path::Path;
use std::time::Duration;

use crate::error::{GitError, Result};
use crate::process::GitCmd;

/// 30 s ceiling on each shellout. Generous — the patch shellout can read
/// non-trivial diffs from disk, and we'd rather time out than wedge the
/// generation indefinitely.
const STAGED_CONTEXT_TIMEOUT: Duration = Duration::from_secs(30);

/// Snapshot of the worktree's staged state, shaped to feed the
/// commit-message-generation prompt builder.
///
/// `summary` and `patch` are returned verbatim as git produced them; the
/// prompt builder is responsible for any further truncation, line-length
/// adjustment, or character-set sanitisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedContext {
    /// Current branch name, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// `git diff --cached --name-status` output (trimmed).
    pub summary: String,
    /// `git diff --cached --patch --minimal --no-color --no-ext-diff` output.
    pub patch: String,
}

/// Fetch the AI commit-message generation context from `workdir`.
///
/// Returns `Ok(None)` when no files are staged — the prompt builder has
/// nothing useful to do in that case, and callers should refuse to spawn
/// the generator (the panel surfaces "No staged changes to summarize.").
///
/// The branch shellout failure is **non-fatal** — a missing branch (fresh
/// repo without HEAD, detached state read failure) just sets
/// `branch: None`. The summary + patch shellouts are fatal because the
/// generator depends on them.
pub async fn fetch(workdir: &Path) -> Result<Option<StagedContext>> {
    let branch_fut = GitCmd::new(workdir)
        .timeout(STAGED_CONTEXT_TIMEOUT)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .run_raw();
    let summary_fut = GitCmd::new(workdir)
        .timeout(STAGED_CONTEXT_TIMEOUT)
        .args(["diff", "--cached", "--name-status"])
        .run();
    let (branch_raw, summary_out) = tokio::try_join!(branch_fut, summary_fut)?;

    let summary = String::from_utf8(summary_out.stdout)
        .map_err(|e| GitError::parse(format!("staged summary not utf-8: {e}")))?
        .trim_end()
        .to_string();
    if summary.is_empty() {
        return Ok(None);
    }

    let branch = if branch_raw.status.success() {
        let raw = String::from_utf8_lossy(&branch_raw.stdout)
            .trim()
            .to_string();
        if raw.is_empty() || raw == "HEAD" {
            None
        } else {
            Some(raw)
        }
    } else {
        None
    };

    let patch_out = GitCmd::new(workdir)
        .timeout(STAGED_CONTEXT_TIMEOUT)
        .args([
            "diff",
            "--cached",
            "--patch",
            "--minimal",
            "--no-color",
            "--no-ext-diff",
        ])
        .run()
        .await?;
    let patch = String::from_utf8(patch_out.stdout)
        .map_err(|e| GitError::parse(format!("staged patch not utf-8: {e}")))?;

    Ok(Some(StagedContext {
        branch,
        summary,
        patch,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .expect("git setup");
        }
        dir
    }

    fn run_git(workdir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(workdir)
            .status()
            .expect("git run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Seed the repo with one committed file so HEAD exists and the
    /// branch ref resolves — `git rev-parse --abbrev-ref HEAD` returns
    /// "HEAD" on a fresh init before any commit, which the helper
    /// folds into `None` (a true detached state can't be distinguished
    /// from pre-first-commit here, but neither needs a branch label).
    fn seed_commit(workdir: &Path) {
        std::fs::write(workdir.join("seed.txt"), "seed").expect("write seed");
        run_git(workdir, &["add", "seed.txt"]);
        run_git(workdir, &["commit", "-m", "seed"]);
    }

    #[tokio::test]
    async fn fetch_returns_none_when_nothing_staged() {
        let dir = init_repo();
        seed_commit(dir.path());
        let result = fetch(dir.path()).await.expect("fetch");
        assert!(result.is_none(), "empty staged area should return None");
    }

    #[tokio::test]
    async fn fetch_returns_context_when_files_staged() {
        let dir = init_repo();
        seed_commit(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").expect("write");
        run_git(dir.path(), &["add", "a.txt"]);

        let context = fetch(dir.path())
            .await
            .expect("fetch")
            .expect("staged content present");
        assert_eq!(context.branch.as_deref(), Some("main"));
        assert!(
            context.summary.contains("a.txt"),
            "summary should mention a.txt: {}",
            context.summary
        );
        assert!(
            context.patch.contains("+hello"),
            "patch should contain added line: {}",
            context.patch
        );
        assert!(
            context.patch.contains("+world"),
            "patch should contain second added line: {}",
            context.patch
        );
    }

    #[tokio::test]
    async fn fetch_handles_detached_head_as_none_branch() {
        let dir = init_repo();
        seed_commit(dir.path());
        run_git(dir.path(), &["checkout", "--detach", "HEAD"]);
        std::fs::write(dir.path().join("b.txt"), "x").expect("write");
        run_git(dir.path(), &["add", "b.txt"]);

        let context = fetch(dir.path())
            .await
            .expect("fetch")
            .expect("staged content present");
        assert_eq!(
            context.branch, None,
            "detached HEAD should yield branch: None"
        );
        assert!(context.summary.contains("b.txt"));
    }

    #[tokio::test]
    async fn fetch_returns_error_when_not_a_git_repo() {
        let dir = TempDir::new().expect("tempdir");
        let result = fetch(dir.path()).await;
        assert!(
            result.is_err(),
            "non-git dir should error, got: {:?}",
            result
        );
    }

    #[test]
    fn staged_context_struct_is_clone_and_eq() {
        let a = StagedContext {
            branch: Some("main".into()),
            summary: "M\ta.txt".into(),
            patch: "diff".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
