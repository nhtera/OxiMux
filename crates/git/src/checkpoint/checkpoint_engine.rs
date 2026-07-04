//! The checkpoint engine: create / restore / compare dangling-commit
//! snapshots of one repository's worktree.

use super::checkpoint_exclude::{ExcludeGuard, MAX_UNTRACKED_BYTES};
use super::checkpoint_temp_index::TempIndexGuard;
use super::{CheckpointError, Result};
use crate::error::GitError;
use crate::process::GitCmd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `git add --all` on a large cold repo can exceed the default 10s budget.
const ADD_ALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSha(pub String);

/// Snapshot engine bound to one repository (resolved from any dir inside it).
///
/// All methods are async (tokio) — GPUI callers must run them via a tokio
/// handle, never on the foreground executor.
#[derive(Debug, Clone)]
pub struct CheckpointEngine {
    repo_root: PathBuf,
    git_dir: PathBuf,
}

impl CheckpointEngine {
    /// `Ok(None)` when `cwd` is not inside a git worktree. `Unsupported` when
    /// the installed git predates `git restore` (< 2.23).
    pub async fn new(cwd: impl AsRef<Path>) -> Result<Option<Self>> {
        let cwd = cwd.as_ref();
        let root = match GitCmd::new(cwd)
            .args(["rev-parse", "--show-toplevel"])
            .run()
            .await
        {
            Ok(out) => PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()),
            Err(GitError::NonZero { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let git_dir_out = GitCmd::new(&root)
            .args(["rev-parse", "--absolute-git-dir"])
            .run()
            .await?;
        let git_dir = PathBuf::from(
            String::from_utf8_lossy(&git_dir_out.stdout)
                .trim()
                .to_string(),
        );

        let version_out = GitCmd::new(&root).arg("--version").run().await?;
        let version = String::from_utf8_lossy(&version_out.stdout).trim().to_string();
        if !supports_restore(&version) {
            return Err(CheckpointError::Unsupported { version });
        }

        Ok(Some(Self { repo_root: root, git_dir }))
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Snapshot the current worktree (tracked + untracked, minus ignored and
    /// oversized-untracked files) as a dangling commit. Never touches the
    /// user's index, HEAD, or refs.
    pub async fn create(&self) -> Result<CheckpointSha> {
        let oversized = self.oversized_untracked().await?;
        let _exclude = ExcludeGuard::new(&self.git_dir, &oversized)?;
        let temp_index = TempIndexGuard::new(&self.git_dir)?;

        // Unborn HEAD (fresh repo) is fine — checkpoint becomes a root commit.
        let head = self
            .cmd()
            .args(["rev-parse", "HEAD"])
            .run()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        self.cmd()
            .env("GIT_INDEX_FILE", temp_index.path())
            .args(["add", "--all"])
            .timeout(ADD_ALL_TIMEOUT)
            .run()
            .await?;
        let tree_out = self
            .cmd()
            .env("GIT_INDEX_FILE", temp_index.path())
            .arg("write-tree")
            .run()
            .await?;
        let tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

        let mut commit = self
            .cmd()
            .env("GIT_INDEX_FILE", temp_index.path())
            .args(["commit-tree", &tree, "-m", "oximux checkpoint"]);
        if let Some(head) = &head {
            commit = commit.args(["-p", head]);
        }
        let sha_out = commit.run().await?;
        Ok(CheckpointSha(
            String::from_utf8_lossy(&sha_out.stdout).trim().to_string(),
        ))
    }

    /// Overwrite worktree files to match the checkpoint tree. The index,
    /// staged state, and branch pointer are untouched; files created AFTER
    /// the checkpoint are NOT deleted (accepted limitation, disclosed in UI).
    pub async fn restore(&self, sha: &CheckpointSha) -> Result<()> {
        let result = self
            .cmd()
            .args(["restore", "--source", &sha.0, "--worktree", "--", "."])
            .run()
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(GitError::NonZero { stderr, .. }) if is_missing_object(&stderr) => {
                Err(CheckpointError::ObjectMissing)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Whether two checkpoints capture different trees.
    pub async fn differs(&self, a: &CheckpointSha, b: &CheckpointSha) -> Result<bool> {
        let raw = self
            .cmd()
            .args(["diff-tree", "--quiet", &a.0, &b.0])
            .run_raw()
            .await?;
        match raw.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => {
                let stderr = String::from_utf8_lossy(&raw.stderr);
                if is_missing_object(&stderr) {
                    Err(CheckpointError::ObjectMissing)
                } else {
                    Err(CheckpointError::Git(GitError::NonZero {
                        code: raw.status.code().unwrap_or(-1),
                        stderr: stderr.trim().to_string(),
                    }))
                }
            }
        }
    }

    /// How many files [`Self::restore`] would overwrite: tracked-side diffs
    /// between the CURRENT worktree and the checkpoint tree. Files created
    /// after the checkpoint (untracked additions) are deliberately NOT
    /// counted — restore leaves them untouched, so they aren't part of the
    /// blast radius this number warns about.
    pub async fn worktree_dirty_since(&self, sha: &CheckpointSha) -> Result<usize> {
        let result = self
            .cmd()
            .args(["diff", "--name-only", &sha.0])
            .run()
            .await;
        match result {
            Ok(out) => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()),
            Err(GitError::NonZero { stderr, .. }) if is_missing_object(&stderr) => {
                Err(CheckpointError::ObjectMissing)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn cmd(&self) -> GitCmd {
        GitCmd::new(&self.repo_root)
            // Untrusted-repo hardening: a repo-local .git/config can point
            // core.hooksPath at an arbitrary script or gpg.program at a
            // blocking binary; GIT_CONFIG_NOSYSTEM alone doesn't cover
            // repo-local config. These -c overrides must precede the
            // subcommand, which they do because cmd() args come first.
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "credential.helper="])
            .args(["-c", "commit.gpgsign=false"])
            .env("GIT_AUTHOR_NAME", "oximux")
            .env("GIT_AUTHOR_EMAIL", "checkpoint@oximux")
            .env("GIT_COMMITTER_NAME", "oximux")
            .env("GIT_COMMITTER_EMAIL", "checkpoint@oximux")
    }

    /// Untracked files >= 2 MiB (repo-root-relative), to exclude from capture.
    async fn oversized_untracked(&self) -> Result<Vec<String>> {
        let out = self
            .cmd()
            .args(["status", "--porcelain=v1", "-uall", "-z"])
            .run()
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut oversized = Vec::new();
        for entry in text.split('\0') {
            // Untracked entries are `?? <path>`; rename entries have a second
            // NUL-separated field we'd mis-parse, but renames are never `??`.
            let Some(path) = entry.strip_prefix("?? ") else {
                continue;
            };
            let abs = self.repo_root.join(path);
            if let Ok(meta) = std::fs::metadata(&abs)
                && meta.is_file()
                && meta.len() >= MAX_UNTRACKED_BYTES
            {
                oversized.push(path.to_string());
            }
        }
        Ok(oversized)
    }
}

fn supports_restore(version_line: &str) -> bool {
    // "git version 2.47.1" / "git version 2.47.1 (Apple Git-153)"
    let Some(rest) = version_line.strip_prefix("git version ") else {
        return false; // unparseable — fail closed, feature off
    };
    let mut parts = rest.split(['.', ' ']);
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    major > 2 || (major == 2 && minor >= 23)
}

fn is_missing_object(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("bad object")
        || s.contains("not a valid object")
        || s.contains("bad revision")
        || s.contains("unable to read tree")
}

#[cfg(test)]
mod tests;
