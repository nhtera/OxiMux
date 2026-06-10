//! Shared transport for forge CLIs (`gh`, `glab`).
//!
//! Both wrappers need the exact same mechanics — off-thread spawn, a
//! non-interactive environment, concurrent stdout/stderr drain, a hard
//! timeout, and `kill_on_drop` so a cancelled future leaves no zombie.
//! Only the binary name and its quiet-mode env vars differ, so they live
//! here once and [`crate::gh::GhCmd`] / [`crate::glab::GlabCmd`] delegate.

use crate::error::{GitError, Result};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Run one forge-CLI invocation to completion. Returns
/// `(success, stdout, stderr)` regardless of exit code so callers can
/// branch on the CLI's status (e.g. `pr view` exiting non-zero == no PR,
/// which is not an error). `envs` carries the binary's non-interactive
/// settings (pager off, prompts off) on top of the shared `LANG`/`LC_ALL`.
pub(crate) async fn run_raw(
    program: &'static str,
    envs: &[(&str, &str)],
    cwd: &Path,
    args: &[OsString],
    timeout: Duration,
) -> Result<(bool, String, String)> {
    let secs = timeout.as_secs();
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd)
        .args(args)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }

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
    let sleep = tokio::time::sleep(timeout);
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

/// True when `origin`'s URL contains `needle` (case-insensitive). The
/// shared body of the per-forge remote checks: uses `git` (not the forge
/// CLI) so detection works even when that CLI is absent, and any failure
/// (no repo, no remote) resolves to `false`.
///
/// `needle` must be lowercase (only the URL is folded). The caller is
/// also responsible for ordering when a URL can match multiple needles —
/// e.g. `github.com/gitlab-tools/x` contains both `github.com` and
/// `gitlab`, which is why forge detection tests GitHub first.
pub(crate) async fn origin_url_contains(cwd: impl AsRef<Path>, needle: &str) -> bool {
    debug_assert_eq!(needle, needle.to_lowercase(), "needle must be lowercase");
    let out = crate::process::GitCmd::new(cwd)
        .args(["remote", "get-url", "origin"])
        .run()
        .await;
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .to_lowercase()
            .contains(needle),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[tokio::test]
    async fn origin_url_contains_matches_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(
            tmp.path(),
            &["remote", "add", "origin", "https://GitLab.example.com/g/r.git"],
        );
        assert!(origin_url_contains(tmp.path(), "gitlab").await);
        assert!(!origin_url_contains(tmp.path(), "github.com").await);
    }

    #[tokio::test]
    async fn origin_url_contains_false_without_remote_or_repo() {
        let no_repo = tempfile::tempdir().unwrap();
        assert!(!origin_url_contains(no_repo.path(), "gitlab").await);

        let no_remote = tempfile::tempdir().unwrap();
        git(no_remote.path(), &["init", "-q"]);
        assert!(!origin_url_contains(no_remote.path(), "gitlab").await);
    }
}
