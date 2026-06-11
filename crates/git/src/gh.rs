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
use oximux_core::PrState;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// exiting non-zero == no open PR, which is not an error). Transport
    /// mechanics live in [`crate::forge_cli::run_raw`]; the env pins gh's
    /// pager and prompts off so it can never block a poll thread.
    pub async fn run_raw(self) -> Result<(bool, String, String)> {
        crate::forge_cli::run_raw(
            "gh",
            &[("GH_PAGER", "cat"), ("GH_PROMPT_DISABLED", "1")],
            &self.cwd,
            &self.args,
            self.timeout,
        )
        .await
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
    crate::forge_cli::origin_url_contains(cwd, "github.com").await
}

/// True when the current branch already has an **open** PR. `gh pr view` exits
/// non-zero only when no PR exists at all — it exits zero for closed and merged
/// PRs too — so an exit code alone would wrongly report a closed PR as open and
/// permanently block re-creating one. Gate on the `state` field instead: gh
/// emits `{"state":"OPEN"}` for an open PR. A spawn/timeout/no-PR result maps to
/// `false` (treat "can't tell" as "no PR" so the button stays usable).
pub async fn has_open_pr(cwd: impl AsRef<Path>) -> bool {
    pr_state(cwd).await.is_open()
}

/// Full PR lifecycle state for the current branch (`OPEN` / `MERGED` /
/// `CLOSED` / none). Same single `gh pr view --json state` round-trip as
/// [`has_open_pr`] — that bool is now derived from this — so callers that
/// need the merged distinction (suppress duplicate Create-PR, Publish
/// "PR Status" variant) pay no extra cost. A spawn/timeout/no-PR result
/// maps to [`PrState::None`] (treat "can't tell" as "no PR").
pub async fn pr_state(cwd: impl AsRef<Path>) -> PrState {
    match GhCmd::new(cwd)
        .args(["pr", "view", "--json", "state"])
        .run_raw()
        .await
    {
        Ok((true, stdout, _)) => parse_pr_state(&stdout),
        _ => PrState::None,
    }
}

/// Extract the PR state from the `gh pr view --json state` JSON
/// (`{"state":"OPEN"}`). Scans for the known state tokens rather than
/// pulling a JSON parser for one field — the same lightweight approach the
/// old `has_open_pr` used for `"OPEN"`, generalised to all states.
fn parse_pr_state(stdout: &str) -> PrState {
    for token in ["OPEN", "MERGED", "CLOSED"] {
        if stdout.contains(&format!("\"{token}\"")) {
            return PrState::from_gh_state(token);
        }
    }
    PrState::None
}

/// Title of one issue / PR by number — `gh issue|pr view N --json title`.
/// `repo` (an `owner/repo` slug from a pasted URL) targets a repository
/// other than `cwd`'s origin via `-R`. Any failure (gh absent, bad number,
/// no network) resolves to `None` — callers prefill nothing and the user's
/// typed text stands.
pub async fn item_title(
    cwd: impl AsRef<Path>,
    kind: oximux_core::ForgeRefKind,
    number: u32,
    repo: Option<&str>,
) -> Option<String> {
    let subcommand = match kind {
        oximux_core::ForgeRefKind::Issue => "issue",
        oximux_core::ForgeRefKind::Pull => "pr",
    };
    let mut cmd = GhCmd::new(cwd).args([
        subcommand,
        "view",
        &number.to_string(),
        "--json",
        "title",
    ]);
    if let Some(slug) = repo {
        cmd = cmd.args(["-R", slug]);
    }
    match cmd.run_raw().await {
        Ok((true, stdout, _)) => parse_title_json(&stdout),
        _ => None,
    }
}

/// Pull the `title` field out of a `--json title` / `-F json` response.
/// Serde (not substring-scanning) so a title that happens to contain
/// `"title"` can't confuse the parse. Shared by the gh and glab fetchers.
pub(crate) fn parse_title_json(stdout: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct TitleView {
        #[serde(default)]
        title: String,
    }
    let v: TitleView = serde_json::from_str(stdout.trim()).ok()?;
    let title = v.title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

/// Options for [`pr_create`]. An empty `title` falls back to `gh pr create
/// --fill` (title + body from the branch's commits); a non-empty `title` is
/// passed explicitly along with `body`. `base` selects the target branch when
/// `Some` (else `gh` auto-detects the default). `draft` opens the PR as a draft.
#[derive(Debug, Clone, Default)]
pub struct CreatePrOptions {
    pub title: String,
    pub body: String,
    pub base: Option<String>,
    pub draft: bool,
}

/// Create a PR for the current branch. With an empty title this is
/// `gh pr create --fill` (title + body derived from the branch's commits);
/// otherwise the supplied title/body/base/draft are passed explicitly. Returns
/// the PR URL on success (the last non-empty `http` line `gh` prints). Errors
/// carry `gh`'s stderr — most usefully "a pull request for branch … already
/// exists".
pub async fn pr_create(cwd: impl AsRef<Path>, opts: CreatePrOptions) -> Result<String> {
    let mut args: Vec<String> = vec!["pr".into(), "create".into()];
    if opts.title.trim().is_empty() {
        args.push("--fill".into());
    } else {
        args.push("--title".into());
        args.push(opts.title);
        args.push("--body".into());
        args.push(opts.body);
    }
    if let Some(base) = opts.base.filter(|b| !b.trim().is_empty()) {
        args.push("--base".into());
        args.push(base);
    }
    if opts.draft {
        args.push("--draft".into());
    }
    let (ok, stdout, stderr) = GhCmd::new(cwd).args(args).run_raw().await?;
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

/// How to merge a PR — maps to the mutually-exclusive `gh pr merge` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    /// `--merge`: a merge commit.
    Merge,
    /// `--squash`: squash all commits into one.
    Squash,
    /// `--rebase`: rebase the commits onto the base.
    Rebase,
}

impl MergeMethod {
    fn flag(self) -> &'static str {
        match self {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

/// Merge the current branch's open PR via `gh pr merge <method>`. Returns unit
/// on success; the error carries `gh`'s stderr (e.g. "not mergeable", "required
/// checks have not passed"). `--delete-branch` is intentionally NOT passed —
/// branch cleanup is the user's call, not a silent side effect of merging.
pub async fn pr_merge(cwd: impl AsRef<Path>, method: MergeMethod) -> Result<()> {
    let (ok, _stdout, stderr) = GhCmd::new(cwd)
        .args(["pr", "merge", method.flag()])
        .run_raw()
        .await?;
    if !ok {
        return Err(GitError::NonZero {
            code: 1,
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(())
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

/// A GitHub label on an issue/PR, from `gh ... list --json labels`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ForgeLabel {
    #[serde(default)]
    pub name: String,
}

/// A GitHub assignee, from `gh ... list --json assignees`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ForgeAssignee {
    #[serde(default)]
    pub login: String,
}

/// One issue or pull request row from `gh issue list` / `gh pr list`. The
/// queried JSON fields are shared between the two, so a single struct covers
/// both kinds; `state` is `OPEN` / `CLOSED` (PRs also report `MERGED`). Every
/// field is `#[serde(default)]` so partial / older JSON still parses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ForgeItem {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub labels: Vec<ForgeLabel>,
    #[serde(default)]
    pub assignees: Vec<ForgeAssignee>,
}

/// Which state to list. Maps to `gh --state <open|closed|all>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForgeState {
    #[default]
    Open,
    Closed,
    All,
}

impl ForgeState {
    fn flag(self) -> &'static str {
        match self {
            ForgeState::Open => "open",
            ForgeState::Closed => "closed",
            ForgeState::All => "all",
        }
    }
}

/// Filter for an issue/PR listing. `mine` adds `--assignee @me`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ForgeListFilter {
    pub state: ForgeState,
    pub mine: bool,
}

/// JSON fields requested for both listings — kept in one place so the two
/// queries stay in lockstep with [`ForgeItem`]'s fields.
const FORGE_LIST_FIELDS: &str = "number,title,state,url,labels,assignees";

/// Cap on rows fetched per listing: a generous single page that keeps the JSON
/// small and the list responsive without paginating.
const FORGE_LIST_LIMIT: &str = "50";

/// List issues for the repo at `cwd` via `gh issue list --json ...`. Returns an
/// empty list (never an error) when `gh` is absent/unauthenticated, the repo
/// isn't a GitHub remote, or the JSON can't be parsed — the Tasks page then
/// shows an empty/guidance state rather than a broken control.
pub async fn issue_list(cwd: impl AsRef<Path>, filter: ForgeListFilter) -> Vec<ForgeItem> {
    forge_list(cwd, "issue", filter).await
}

/// List pull requests for the repo at `cwd` via `gh pr list --json ...`. Same
/// graceful-degradation contract as [`issue_list`].
pub async fn pr_list(cwd: impl AsRef<Path>, filter: ForgeListFilter) -> Vec<ForgeItem> {
    forge_list(cwd, "pr", filter).await
}

async fn forge_list(cwd: impl AsRef<Path>, kind: &str, filter: ForgeListFilter) -> Vec<ForgeItem> {
    let mut cmd = GhCmd::new(cwd)
        .args([
            kind,
            "list",
            "--json",
            FORGE_LIST_FIELDS,
            "--state",
            filter.state.flag(),
            "--limit",
            FORGE_LIST_LIMIT,
        ])
        .timeout(Duration::from_secs(15));
    if filter.mine {
        cmd = cmd.args(["--assignee", "@me"]);
    }
    let Ok((_ok, stdout, _stderr)) = cmd.run_raw().await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<ForgeItem>>(stdout.trim()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_title_json_reads_gh_shape() {
        assert_eq!(
            parse_title_json(r#"{"title":"Fix crash"}"#),
            Some("Fix crash".to_string())
        );
        assert_eq!(parse_title_json(r#"{"title":"  "}"#), None);
        assert_eq!(parse_title_json("not json"), None);
    }

    #[test]
    fn parse_title_json_tolerates_glab_full_object() {
        // glab's `-F json` returns the whole object, not just the
        // requested field — serde must ignore the rest.
        let json = r#"{"id":1,"iid":42,"title":"Fix crash","description":"x","state":"opened"}"#;
        assert_eq!(parse_title_json(json), Some("Fix crash".to_string()));
    }

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

    #[test]
    fn parses_forge_items_json() {
        let json = r#"[
            {"number":42,"title":"Fix crash","state":"OPEN","url":"https://x/42",
             "labels":[{"name":"bug"},{"name":"p1"}],"assignees":[{"login":"alice"}]},
            {"number":7,"title":"Docs","state":"CLOSED","url":"https://x/7",
             "labels":[],"assignees":[]}
        ]"#;
        let items: Vec<ForgeItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].number, 42);
        assert_eq!(items[0].title, "Fix crash");
        assert_eq!(items[0].state, "OPEN");
        assert_eq!(items[0].labels.len(), 2);
        assert_eq!(items[0].labels[1].name, "p1");
        assert_eq!(items[0].assignees[0].login, "alice");
        assert!(items[1].assignees.is_empty());
    }

    #[test]
    fn forge_items_json_tolerates_missing_fields() {
        // Partial JSON (only number+title) still parses via serde defaults.
        let json = r#"[{"number":3,"title":"bare"}]"#;
        let items: Vec<ForgeItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items[0].number, 3);
        assert!(items[0].url.is_empty());
        assert!(items[0].labels.is_empty());
    }

    #[test]
    fn forge_state_flag_maps_to_gh_values() {
        assert_eq!(ForgeState::Open.flag(), "open");
        assert_eq!(ForgeState::Closed.flag(), "closed");
        assert_eq!(ForgeState::All.flag(), "all");
        assert_eq!(ForgeState::default(), ForgeState::Open);
    }

    #[tokio::test]
    async fn issue_list_empty_outside_repo() {
        // No repo / no `gh` → graceful empty list, never panics.
        let tmp = tempfile::tempdir().unwrap();
        assert!(issue_list(tmp.path(), ForgeListFilter::default()).await.is_empty());
    }
}
