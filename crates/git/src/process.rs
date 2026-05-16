//! Tokio-based wrapper around `git` invocations.
//!
//! Goals:
//! - All git I/O off the GPUI main thread (callers `await` on a tokio runtime).
//! - Hard timeout so a misbehaving git command can't deadlock the UI.
//! - `kill_on_drop` so a cancelled future doesn't leave a zombie git process.
//! - Locale-stable stderr (`LANG=C`) for predictable error messages.

use crate::error::{GitError, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builder for a single `git` invocation.
#[derive(Debug, Clone)]
pub struct GitCmd {
    cwd: PathBuf,
    args: Vec<OsString>,
    timeout: Duration,
}

/// Successful git output (exit code zero). Stderr is dropped on success.
#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: Vec<u8>,
}

/// Raw git output regardless of exit code. For callers that legitimately
/// need to inspect non-zero exits (e.g. `git merge` returns 1 on conflict).
#[derive(Debug, Clone)]
pub struct RawOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

impl GitCmd {
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

    /// Run to completion, requiring exit code zero.
    pub async fn run(self) -> Result<Output> {
        let raw = self.run_raw().await?;
        if !raw.status.success() {
            return Err(GitError::NonZero {
                code: raw.status.code().unwrap_or(-1),
                stderr: trim_stderr(&raw.stderr),
            });
        }
        Ok(Output { stdout: raw.stdout })
    }

    /// Run to completion, returning the raw exit + stderr unconditionally.
    pub async fn run_raw(self) -> Result<RawOutput> {
        let secs = self.timeout.as_secs();
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.cwd)
            .args(&self.args)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            // Don't let a user-side config pop a credential helper / pager.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            // StatusPoller polls every 500 ms; optional lock contention against
            // concurrent foreground git ops produces spurious NonZero errors.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Sandbox: ignore /etc/gitconfig (which can set `core.hooksPath` to
            // an arbitrary script and run it on every status poll).
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // NB: `tokio::process::Command::spawn` returns ErrorKind::NotFound for
        // both a missing `git` binary AND a missing `current_dir` — we can't
        // disambiguate from the io::Error alone. Surface the io error and let
        // higher layers (e.g. `Repository::open`) classify after checking
        // whether the cwd exists.
        let child = cmd.spawn().map_err(GitError::Spawn)?;
        match timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => Ok(RawOutput {
                stdout: out.stdout,
                stderr: out.stderr,
                status: out.status,
            }),
            Ok(Err(e)) => Err(GitError::Spawn(e)),
            Err(_elapsed) => Err(GitError::Timeout { secs }),
        }
    }
}

fn trim_stderr(buf: &[u8]) -> String {
    let s = String::from_utf8_lossy(buf);
    let trimmed = s.trim();
    // Keep stderr short in errors; full text is in tracing. Iterate by char to
    // avoid panicking on a multibyte codepoint boundary (paths with non-ASCII).
    if trimmed.len() > 512 {
        let mut out: String = trimmed.chars().take(512).collect();
        out.push('…');
        out
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn version_smoke() {
        let out = GitCmd::new(std::env::current_dir().unwrap())
            .arg("--version")
            .run()
            .await
            .expect("git --version should work in CI");
        let s = String::from_utf8(out.stdout).unwrap();
        assert!(s.starts_with("git version"), "got: {s:?}");
    }

    #[tokio::test]
    async fn non_zero_surfaces_stderr() {
        // `git rev-parse --git-dir` outside any repo exits non-zero.
        let tmp = tempfile::tempdir().unwrap();
        let err = GitCmd::new(tmp.path())
            .args(["rev-parse", "--git-dir"])
            .run()
            .await
            .expect_err("should fail outside a repo");
        match err {
            GitError::NonZero { stderr, .. } => {
                assert!(stderr.contains("not a git repository"), "stderr: {stderr}")
            }
            other => panic!("expected NonZero, got {other:?}"),
        }
    }
}
