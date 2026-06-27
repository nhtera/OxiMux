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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builder for a single `git` invocation.
#[derive(Debug, Clone)]
pub struct GitCmd {
    cwd: PathBuf,
    args: Vec<OsString>,
    timeout: Duration,
    /// If `Some`, bytes piped to the child's stdin (and stdin handle closed
    /// after, so the child sees EOF). If `None`, stdin is `/dev/null`.
    stdin_bytes: Option<Vec<u8>>,
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
            stdin_bytes: None,
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

    /// Pipe `data` to the child's stdin, then close it. The handle is dropped
    /// **before** stdout/stderr drain begins to prevent deadlock against
    /// commands that read all stdin before producing any output (`git apply`,
    /// `git hash-object --stdin`, …).
    pub fn stdin(mut self, data: Vec<u8>) -> Self {
        self.stdin_bytes = Some(data);
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
        let stdin_bytes = self.stdin_bytes;
        let stdin_mode = if stdin_bytes.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        };
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.cwd)
            // Emit non-ASCII paths verbatim (UTF-8) instead of git's default
            // octal-escaped `"\NNN"` quoting. Without this, a filename with
            // Vietnamese/CJK/accented characters comes back quoted in `diff`
            // and `log` output and the path parsers (which strip a bare `a/`
            // prefix) silently drop the file. This is a `git`-level option, so
            // it MUST precede the subcommand in `self.args`.
            .arg("-c")
            .arg("core.quotePath=false")
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
            .stdin(stdin_mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // NB: `tokio::process::Command::spawn` returns ErrorKind::NotFound for
        // both a missing `git` binary AND a missing `current_dir` — we can't
        // disambiguate from the io::Error alone. Surface the io error and let
        // higher layers (e.g. `Repository::open`) classify after checking
        // whether the cwd exists.
        let mut child = cmd.spawn().map_err(GitError::spawn)?;

        // Write stdin (if any) and drop the handle BEFORE draining stdout —
        // commands like `git apply -` consume all stdin before producing
        // output, so the order matters or we'd deadlock.
        if let Some(bytes) = stdin_bytes {
            let mut stdin_pipe = child.stdin.take().expect("stdin was piped above");
            if let Err(e) = stdin_pipe.write_all(&bytes).await {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(GitError::spawn(e));
            }
            drop(stdin_pipe);
        }

        // Drive child manually rather than `wait_with_output()` so we retain
        // `&mut child` for explicit kill+reap on the timeout path. Letting the
        // tokio runtime's SIGCHLD reaper pick up an abandoned zombie was the
        // v0.9 orphan-process failure class.
        let mut stdout_pipe = child
            .stdout
            .take()
            .expect("stdout was piped in command builder");
        let mut stderr_pipe = child
            .stderr
            .take()
            .expect("stderr was piped in command builder");

        // Buffers live inside the future so they're not borrowed across the
        // select! arm boundary. Future returns them by value on success.
        let drain = async move {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            tokio::try_join!(
                stdout_pipe.read_to_end(&mut stdout_buf),
                stderr_pipe.read_to_end(&mut stderr_buf),
            )?;
            std::io::Result::Ok((stdout_buf, stderr_buf))
        };
        let sleep = tokio::time::sleep(self.timeout);
        tokio::pin!(drain);
        tokio::pin!(sleep);

        tokio::select! {
            biased;
            _ = &mut sleep => {
                // Explicit cleanup so the reap happens before this future returns.
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(GitError::Timeout { secs })
            }
            drained = &mut drain => {
                let (stdout_buf, stderr_buf) = drained.map_err(GitError::spawn)?;
                let status = child.wait().await.map_err(GitError::spawn)?;
                Ok(RawOutput {
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                    status,
                })
            }
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

    #[tokio::test]
    async fn stdin_pipes_into_hash_object() {
        // `git hash-object --stdin` reads stdin and prints the blob SHA-1.
        // For "hello\n" the SHA is the well-known `ce0136…`. Exercises both
        // the stdin builder and the drop-before-drain ordering.
        let out = GitCmd::new(std::env::current_dir().unwrap())
            .args(["hash-object", "--stdin"])
            .stdin(b"hello\n".to_vec())
            .run()
            .await
            .expect("hash-object --stdin should work");
        let sha = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            sha.trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a",
            "unexpected blob sha"
        );
    }
}
