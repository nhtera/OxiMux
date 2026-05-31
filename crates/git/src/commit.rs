//! Commit operations on `Repository`: `commit` (staged changes only) and
//! `commit_paths` (stage-and-commit a specific path list). Both return the
//! new HEAD SHA on success. Hooks run; `--no-verify` is intentionally not
//! exposed (out of scope for v1).

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use std::path::Path;

impl Repository {
    /// Commit currently-staged changes. `message` is the full commit message
    /// (subject + blank line + body — caller assembles).
    ///
    /// Returns the new HEAD SHA on success. `InvalidInput` on empty /
    /// whitespace-only message (rejected client-side so the UI doesn't have
    /// to parse git's generic stderr). `NonZero` from git when nothing is
    /// staged (empty commit), pre-commit hook fails, etc.
    pub async fn commit(&self, message: &str) -> Result<String> {
        validate_message(message)?;
        GitCmd::new(self.workdir())
            .args(["commit", "-m", message])
            .run()
            .await?;
        self.head_sha().await
    }

    /// Stage-and-commit specific files in one operation.
    ///
    /// **WARNING:** the pre-stage step (`git add -- <paths>`) overwrites any
    /// existing hunk-level partial staging on those paths with the full
    /// worktree content. If the caller had run `stage_hunks()` to stage only
    /// selected hunks on a path, `commit_paths()` on that same path will
    /// commit MORE than the original selection. Callers mixing hunk-staging
    /// with `commit_paths` should use `commit()` (staged-only) instead.
    ///
    /// Independently-staged changes on OTHER files (not in `paths`) stay in
    /// the index — the trailing `-- <paths>` on commit constrains it.
    ///
    /// Internally: `git add -- <paths>` then `git commit -m <msg> -- <paths>`.
    /// The pre-stage is required because `git commit -- <paths>` alone only
    /// commits already-tracked files (pathspec error on untracked).
    ///
    /// Returns the new HEAD SHA on success. `InvalidInput` if message is
    /// empty/whitespace-only or `paths` is empty.
    pub async fn commit_paths(&self, message: &str, paths: &[&Path]) -> Result<String> {
        validate_message(message)?;
        if paths.is_empty() {
            return Err(GitError::invalid_input(
                "commit_paths requires at least one path; use commit() for staged-only",
            ));
        }
        // Pre-stage so untracked paths are committable too. `git commit -- <paths>`
        // alone only commits tracked files.
        self.stage_paths(paths).await?;
        let mut cmd = GitCmd::new(self.workdir()).args(["commit", "-m", message, "--"]);
        for p in paths {
            cmd = cmd.arg(p.as_os_str());
        }
        cmd.run().await?;
        self.head_sha().await
    }

    /// `git cherry-pick <sha>` — re-apply one historical commit onto HEAD.
    ///
    /// Returns `Ok(())` once cherry-pick exits cleanly (the new commit
    /// lands on HEAD). On conflict, git exits non-zero and leaves the
    /// worktree in CHERRY_PICK_HEAD state; the caller surfaces this via
    /// the operation banner (already wired through
    /// `Repository::current_operation()`), so the resulting `Err` carries
    /// raw stderr for the status row.
    ///
    /// `sha` is forwarded verbatim — short hashes and named refs are both
    /// valid input (`git cherry-pick HEAD~3` works) but the UI only ever
    /// passes a full 40-char OID from the commit graph row.
    pub async fn cherry_pick(&self, sha: &str) -> Result<()> {
        if sha.trim().is_empty() {
            return Err(GitError::invalid_input("cherry_pick sha is empty"));
        }
        GitCmd::new(self.workdir())
            .args(["cherry-pick", sha])
            .run()
            .await?;
        Ok(())
    }

    /// `git revert --no-edit <sha>` — append an inverse commit that undoes
    /// the named one. `--no-edit` skips the editor (matches GUI tool
    /// behavior); the commit message defaults to git's auto-generated
    /// "Revert ‹original subject›".
    ///
    /// Same conflict + status surfacing as `cherry_pick` — a conflicted
    /// revert leaves the worktree in REVERT_HEAD state and surfaces
    /// through the operation banner. Returns raw stderr in the `Err` for
    /// the status row.
    pub async fn revert_commit(&self, sha: &str) -> Result<()> {
        if sha.trim().is_empty() {
            return Err(GitError::invalid_input("revert_commit sha is empty"));
        }
        GitCmd::new(self.workdir())
            .args(["revert", "--no-edit", sha])
            .run()
            .await?;
        Ok(())
    }

    /// Full HEAD SHA. Tiny helper used by `commit` / `commit_paths` to surface
    /// the freshly-minted commit. `git rev-parse HEAD` is locale-stable and
    /// doesn't depend on the noisy `git commit` stdout format.
    pub(crate) async fn head_sha(&self) -> Result<String> {
        let out = GitCmd::new(self.workdir())
            .args(["rev-parse", "HEAD"])
            .run()
            .await?;
        let sha = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 from `git rev-parse HEAD`: {e}")))?
            .trim()
            .to_string();
        if sha.is_empty() {
            return Err(GitError::parse("empty HEAD sha from `git rev-parse HEAD`"));
        }
        Ok(sha)
    }
}

fn validate_message(message: &str) -> Result<()> {
    if message.trim().is_empty() {
        return Err(GitError::invalid_input(
            "commit message is empty or whitespace-only",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_message("").is_err());
    }

    #[test]
    fn validate_rejects_whitespace_only() {
        for bad in [" ", "\n", "\t  \n", "   \n\n   "] {
            assert!(validate_message(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn validate_accepts_normal_messages() {
        for ok in ["x", "fix: foo", "feat: bar\n\nbody"] {
            validate_message(ok).unwrap_or_else(|e| panic!("{ok:?} should pass: {e:?}"));
        }
    }
}
