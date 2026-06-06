//! Tokio-based wrapper around the GitHub CLI (`gh`).
//!
//! Mirrors [`crate::process::GitCmd`] in spirit — off-thread invocation, a hard
//! timeout, and `kill_on_drop` so a cancelled future leaves no zombie — but
//! spawns `gh` instead of `git`. GitHub-only: `gh` does not understand GitLab /
//! Bitbucket remotes, so callers gate the Create-PR surface on a GitHub remote
//! (see [`is_github_remote`]).
//!
//! Kept deliberately small: PR creation uses `gh pr create --fill`, which
//! populates the title + body from the branch's commits (matching "PR text =
//! latest commit subject/body, no AI"), so no commit-message plumbing is
//! needed here. Existing-PR detection is exit-code based (`gh pr view` exits
//! non-zero when the branch has no open PR), so no JSON parser is pulled in.

use crate::error::{GitError, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// `gh` can reach the network (auth, PR create), so allow more headroom than
/// the local-git default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Builder for a single `gh` invocation.
#[derive(Debug, Clone)]
pub struct GhCmd {
    cwd: PathBuf,
    args: Vec<OsString>,
    timeout: Duration,
}

impl GhCmd {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            args: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|s| s.as_ref().to_os_string()));
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Run to completion. Returns `(success, stdout, stderr)` regardless of
    /// exit code so callers can branch on `gh`'s exit status (e.g. `pr view`
    /// exiting non-zero == no open PR, which is not an error).
    pub async fn run_raw(self) -> Result<(bool, String, String)> {
        let secs = self.timeout.as_secs();
        let mut cmd = Command::new("gh");
        cmd.current_dir(&self.cwd)
            .args(&self.args)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            // Never let gh open an interactive pager / prompt on a poll thread.
            .env("GH_PAGER", "cat")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(GitError::spawn)?;
        let mut stdout_pipe = child.stdout.take().expect("stdout piped above");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped above");

        let drain = async move {
            let mut out = Vec::new();
            let mut err = Vec::new();
            tokio::try_join!(
                stdout_pipe.read_to_end(&mut out),
                stderr_pipe.read_to_end(&mut err),
            )?;
            std::io::Result::Ok((out, err))
        };
        let sleep = tokio::time::sleep(self.timeout);
        tokio::pin!(drain);
        tokio::pin!(sleep);

        tokio::select! {
            biased;
            _ = &mut sleep => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(GitError::Timeout { secs })
            }
            drained = &mut drain => {
                let (out, err) = drained.map_err(GitError::spawn)?;
                let status = child.wait().await.map_err(GitError::spawn)?;
                Ok((
                    status.success(),
                    String::from_utf8_lossy(&out).into_owned(),
                    String::from_utf8_lossy(&err).into_owned(),
                ))
            }
        }
    }
}

/// True when the GitHub CLI is installed and authenticated. A single
/// `gh auth status` round-trip; any failure (missing binary, not logged in)
/// resolves to `false` so the caller can show install/auth guidance instead
/// of a broken button.
pub async fn available(cwd: impl AsRef<Path>) -> bool {
    matches!(
        GhCmd::new(cwd)
            .args(["auth", "status"])
            .timeout(Duration::from_secs(8))
            .run_raw()
            .await,
        Ok((true, ..))
    )
}

/// True when `origin` points at github.com. `gh` only supports GitHub, so the
/// Create-PR affordance is hidden for other hosts. Uses `git` (not `gh`) so it
/// works even when `gh` is absent. Any failure resolves to `false`.
pub async fn is_github_remote(cwd: impl AsRef<Path>) -> bool {
    let out = crate::process::GitCmd::new(cwd)
        .args(["remote", "get-url", "origin"])
        .run()
        .await;
    match out {
        Ok(o) => {
            let url = String::from_utf8_lossy(&o.stdout).to_lowercase();
            url.contains("github.com")
        }
        Err(_) => false,
    }
}

/// True when the current branch already has an **open** PR. `gh pr view` exits
/// non-zero only when no PR exists at all — it exits zero for closed and merged
/// PRs too — so an exit code alone would wrongly report a closed PR as open and
/// permanently block re-creating one. Gate on the `state` field instead: gh
/// emits `{"state":"OPEN"}` for an open PR. A spawn/timeout/no-PR result maps to
/// `false` (treat "can't tell" as "no PR" so the button stays usable).
pub async fn has_open_pr(cwd: impl AsRef<Path>) -> bool {
    match GhCmd::new(cwd)
        .args(["pr", "view", "--json", "state"])
        .run_raw()
        .await
    {
        Ok((true, stdout, _)) => stdout.contains("\"OPEN\""),
        _ => false,
    }
}

/// Create a PR for the current branch via `gh pr create --fill` (title + body
/// derived from the branch's commits). Returns the PR URL on success (the URL
/// is the last non-empty line `gh` prints). Errors carry `gh`'s stderr — most
/// usefully "a pull request for branch … already exists".
pub async fn pr_create(cwd: impl AsRef<Path>) -> Result<String> {
    let (ok, stdout, stderr) = GhCmd::new(cwd)
        .args(["pr", "create", "--fill"])
        .run_raw()
        .await?;
    if !ok {
        return Err(GitError::NonZero {
            code: 1,
            stderr: stderr.trim().to_string(),
        });
    }
    // gh prints progress lines then the URL; take the last URL-looking line.
    // Fall back to empty (not raw stdout) when no URL line is present so the
    // browser-open step's empty guard skips rather than launching garbage.
    let url = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("http"))
        .unwrap_or("")
        .to_string();
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_accumulates_args() {
        let cmd = GhCmd::new("/tmp").args(["pr", "view"]).arg("--json");
        assert_eq!(cmd.args, ["pr", "view", "--json"]);
        assert_eq!(cmd.cwd, PathBuf::from("/tmp"));
    }

    #[test]
    fn builder_overrides_timeout() {
        let cmd = GhCmd::new("/tmp").timeout(Duration::from_secs(5));
        assert_eq!(cmd.timeout, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn is_github_remote_false_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_github_remote(tmp.path()).await);
    }

    #[tokio::test]
    async fn has_open_pr_false_outside_repo() {
        // No repo, or `gh` absent → non-zero/err → false, never panics.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!has_open_pr(tmp.path()).await);
    }
}
