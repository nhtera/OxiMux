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
use serde::Deserialize;
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

/// One CI check run for the branch's PR, as reported by `gh pr checks --json`.
/// `bucket` is gh's coarse category — one of `pass`, `fail`, `pending`,
/// `skipping`, `cancel` — which is exactly the granularity the compact CI row
/// needs (the per-provider `state` strings vary too much to switch on).
///
/// `link` is the web URL of the run (used to open in a browser and to extract
/// the run id for a log peek); `description` is gh's short human blurb (e.g.
/// "Successful in 2m"). Both are `#[serde(default)]` so older / partial JSON
/// still parses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub description: String,
}

/// Cap a check-run log peek so a multi-megabyte CI log can't blow the prompt
/// budget or the panel. The tail is the most useful part of a failure log
/// (the error + stack), so callers keep the trailing bytes.
pub const CHECK_LOG_BYTE_BUDGET: usize = 16_000;

/// Fetch the CI check runs for the current branch's PR via
/// `gh pr checks --json name,bucket,link,description`. Returns an empty list
/// (never an error) when there is no PR, no checks, `gh` is
/// absent/unauthenticated, or the JSON can't be parsed — the CI row simply
/// doesn't render in those cases. NB: `gh pr checks` exits non-zero when checks
/// are pending/failing, so the JSON is parsed from stdout regardless of exit
/// code.
pub async fn pr_checks(cwd: impl AsRef<Path>) -> Vec<CheckRun> {
    let Ok((_ok, stdout, _stderr)) = GhCmd::new(cwd)
        .args(["pr", "checks", "--json", "name,bucket,link,description"])
        // Shorter than the default: this runs on the SCM poll path right after
        // has_open_pr, so cap the sequential worst case rather than stacking two
        // 30s timeouts behind git-status updates.
        .timeout(Duration::from_secs(10))
        .run_raw()
        .await
    else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<CheckRun>>(stdout.trim()).unwrap_or_default()
}

/// Extract the workflow-run id from a check run's `link` URL. GitHub Actions
/// check links look like `https://github.com/o/r/actions/runs/<id>/job/<job>`;
/// the `runs/<id>` segment is what `gh run view` needs. Returns `None` for
/// non-Actions checks (external status contexts) whose links carry no run id.
pub fn run_id_from_link(link: &str) -> Option<u64> {
    let after = link.split("/runs/").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Fetch the failed-job log for one workflow run via
/// `gh run view <run_id> --log-failed`, returning the trailing
/// [`CHECK_LOG_BYTE_BUDGET`] bytes (the tail holds the actual error). Returns
/// `None` when `gh` is absent, the run has no failed jobs, or the call times
/// out — the caller then shows a "no log available" state rather than an error.
pub async fn run_log(cwd: impl AsRef<Path>, run_id: u64) -> Option<String> {
    let (ok, stdout, _stderr) = GhCmd::new(cwd)
        .args(["run", "view", &run_id.to_string(), "--log-failed"])
        .timeout(Duration::from_secs(20))
        .run_raw()
        .await
        .ok()?;
    // `gh run view --log-failed` exits non-zero when the run had failures —
    // which is exactly when the log is worth showing — so the log is read from
    // stdout regardless of exit code. Only a non-zero exit with *no* output
    // (e.g. bad run id, no failed jobs) counts as "nothing to show".
    if !ok && stdout.trim().is_empty() {
        return None;
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(tail_bytes(trimmed, CHECK_LOG_BYTE_BUDGET))
}

/// Keep the last `budget` bytes of `s`, walking forward to a char boundary so
/// the slice never splits a multi-byte codepoint. Prepends an elision marker
/// when truncation happened.
fn tail_bytes(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mut start = s.len() - budget;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(earlier log lines omitted)\n{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checks_json() {
        let json = r#"[{"name":"build","bucket":"pass"},{"name":"test","bucket":"fail"}]"#;
        let runs: Vec<CheckRun> = serde_json::from_str(json).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].name, "build");
        assert_eq!(runs[1].bucket, "fail");
    }

    #[test]
    fn checks_json_tolerates_unknown_fields() {
        let json = r#"[{"name":"lint","bucket":"pending","link":"http://x","extra":1}]"#;
        let runs: Vec<CheckRun> = serde_json::from_str(json).unwrap();
        assert_eq!(runs[0].bucket, "pending");
        assert_eq!(runs[0].link, "http://x");
    }

    #[test]
    fn checks_json_parses_link_and_description() {
        let json = r#"[{"name":"build","bucket":"fail","link":"https://github.com/o/r/actions/runs/42/job/7","description":"Failing after 1m"}]"#;
        let runs: Vec<CheckRun> = serde_json::from_str(json).unwrap();
        assert_eq!(runs[0].description, "Failing after 1m");
        assert!(runs[0].link.contains("/runs/42/"));
    }

    #[test]
    fn run_id_parses_from_actions_link() {
        assert_eq!(
            run_id_from_link("https://github.com/o/r/actions/runs/123456/job/789"),
            Some(123456)
        );
        assert_eq!(
            run_id_from_link("https://github.com/o/r/actions/runs/42"),
            Some(42)
        );
    }

    #[test]
    fn run_id_none_for_non_actions_link() {
        assert_eq!(run_id_from_link("https://example.com/status/build"), None);
        assert_eq!(run_id_from_link(""), None);
        // Malformed-but-plausible: `/runs/` present but no parseable id.
        assert_eq!(run_id_from_link("https://github.com/o/r/actions/runs/"), None);
        assert_eq!(
            run_id_from_link("https://github.com/o/r/actions/runs/abc/job/1"),
            None
        );
    }

    #[test]
    fn tail_bytes_keeps_trailing_within_budget() {
        assert_eq!(tail_bytes("short", 100), "short");
    }

    #[test]
    fn tail_bytes_truncates_to_tail_with_marker() {
        let s = "A".repeat(50);
        let out = tail_bytes(&s, 10);
        assert!(out.contains("omitted"));
        assert!(out.ends_with(&"A".repeat(10)));
    }

    #[test]
    fn tail_bytes_respects_utf8_boundary() {
        // Each 'é' is 2 bytes; an odd budget must not split a codepoint.
        let s = "é".repeat(100);
        let out = tail_bytes(&s, 51);
        // Must be valid UTF-8 (no panic on slicing) and carry the marker.
        assert!(out.contains("omitted"));
    }

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
