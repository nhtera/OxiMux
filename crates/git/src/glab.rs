//! Tokio-based wrapper around the GitLab CLI (`glab`).
//!
//! The GitLab counterpart to [`crate::gh`]: same off-thread invocation, hard
//! timeout, and `kill_on_drop` discipline, but spawns `glab` and speaks GitLab's
//! merge-request vocabulary (`glab mr ...`). GitLab-only — callers gate the
//! MR surface on a GitLab remote (see [`is_gitlab_remote`]).
//!
//! The forge-contract *data types* (`CreatePrOptions`, `MergeMethod`,
//! `ForgeItem`, `ForgeListFilter`) are shared with [`crate::gh`] and re-used
//! here; only the command shapes and GitLab's JSON differ. GitLab's list JSON
//! diverges from GitHub's (`iid`/`web_url`, string labels), so a private
//! [`GitlabItem`] maps it onto the shared [`crate::gh::ForgeItem`].
//!
//! Graceful degradation matches `gh`: a "can't tell" result (binary absent, no
//! MR, parse failure) resolves to a benign default, never an error.

use crate::error::{GitError, Result};
use crate::gh::{
    CreatePrOptions, ForgeAssignee, ForgeAuthor, ForgeItem, ForgeLabel, ForgeListFilter, ItemDetail,
    MergeMethod,
};
use oximux_core::PrState;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `glab` reaches the network (auth, MR create), so allow the same headroom as
/// the `gh` default rather than the local-git timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Builder for a single `glab` invocation. Mirrors [`crate::gh::GhCmd`] — the
/// transport mechanics (spawn, timeout, drain, kill-on-drop) are identical; the
/// binary and its non-interactive env differ.
#[derive(Debug, Clone)]
pub struct GlabCmd {
    cwd: PathBuf,
    args: Vec<OsString>,
    timeout: Duration,
}

impl GlabCmd {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            args: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
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

    /// Run to completion, returning `(success, stdout, stderr)` regardless of
    /// exit code so callers can branch on `glab`'s status (e.g. `mr view`
    /// exiting non-zero == no MR, which is not an error). Transport
    /// mechanics live in [`crate::forge_cli::run_raw`]; the env pins glab's
    /// pager, update check, and telemetry off so a poll thread never blocks
    /// (every mutating call also passes an explicit `--yes`).
    pub async fn run_raw(self) -> Result<(bool, String, String)> {
        crate::forge_cli::run_raw(
            "glab",
            &[
                ("NO_COLOR", "1"),
                ("PAGER", "cat"),
                ("GLAB_CHECK_UPDATE", "0"),
                ("GLAB_SEND_TELEMETRY", "0"),
            ],
            &self.cwd,
            &self.args,
            self.timeout,
        )
        .await
    }
}

/// True when `origin` points at a GitLab host. Uses `git` (not `glab`) so it
/// works even when `glab` is absent. Matches the `gitlab` substring, which
/// covers `gitlab.com` and the common self-hosted `gitlab.<company>.tld`
/// pattern; an idiosyncratic self-hosted domain that omits "gitlab" won't be
/// detected (documented limitation — such repos fall through to no forge). Any
/// failure resolves to `false`.
pub async fn is_gitlab_remote(cwd: impl AsRef<Path>) -> bool {
    crate::forge_cli::origin_url_contains(cwd, "gitlab").await
}

/// Full MR lifecycle state for the current branch. One `glab mr view -F json`
/// round-trip; the `has_open_mr` bool derives from it. A spawn/timeout/no-MR
/// result maps to [`PrState::None`] ("can't tell" == "no MR").
pub async fn mr_state(cwd: impl AsRef<Path>) -> PrState {
    match GlabCmd::new(cwd)
        .args(["mr", "view", "-F", "json"])
        .run_raw()
        .await
    {
        Ok((true, stdout, _)) => parse_mr_state(&stdout),
        _ => PrState::None,
    }
}

/// The single field we need from `glab mr view -F json`. Deserializing just the
/// `state` key (serde ignores the rest) is robust against an MR whose title,
/// branch name, or a label literally spells a state word — a naive substring
/// scan of the whole JSON would misread those.
#[derive(Debug, Deserialize)]
struct MrStateView {
    #[serde(default)]
    state: String,
}

/// True when the current branch already has an **open** MR. Derived from
/// [`mr_state`] so the single round-trip is shared.
pub async fn has_open_mr(cwd: impl AsRef<Path>) -> bool {
    mr_state(cwd).await.is_open()
}

/// Title of one issue / MR by number — `glab issue|mr view N -F json`.
/// `repo` (a `group/project` path from a pasted URL) targets a repository
/// other than `cwd`'s origin via `--repo`. Any failure (glab absent, bad
/// number, no network) resolves to `None` — callers prefill nothing and
/// the user's typed text stands.
pub async fn item_title(
    cwd: impl AsRef<Path>,
    kind: oximux_core::ForgeRefKind,
    number: u32,
    repo: Option<&str>,
) -> Option<String> {
    let subcommand = match kind {
        oximux_core::ForgeRefKind::Issue => "issue",
        oximux_core::ForgeRefKind::Pull => "mr",
    };
    let mut cmd = GlabCmd::new(cwd).args([
        subcommand,
        "view",
        &number.to_string(),
        "-F",
        "json",
    ]);
    if let Some(path) = repo {
        cmd = cmd.args(["--repo", path]);
    }
    match cmd.run_raw().await {
        Ok((true, stdout, _)) => crate::gh::parse_title_json(&stdout),
        _ => None,
    }
}

/// Extract the MR state from `glab mr view -F json` (`{"state":"opened",...}`).
/// Parses the `state` field with serde so only the actual state value is read
/// — a lowercase GitLab state word (`opened`/`merged`/`closed`/`locked`) is
/// common enough in titles/branch names that a whole-document substring scan
/// would misclassify. Unparseable JSON → [`PrState::None`].
fn parse_mr_state(stdout: &str) -> PrState {
    serde_json::from_str::<MrStateView>(stdout.trim())
        .map(|v| PrState::from_gitlab_state(&v.state))
        .unwrap_or(PrState::None)
}

/// Create an MR for the current branch via `glab mr create`. With an empty
/// title this is `--fill` (title + description from the branch's commits);
/// otherwise the supplied title/description/target/draft are passed. `--yes`
/// keeps it non-interactive. Returns the MR URL on success (the last `http`
/// line `glab` prints); the error carries `glab`'s stderr (commonly "open merge
/// request already exists").
pub async fn mr_create(cwd: impl AsRef<Path>, opts: CreatePrOptions) -> Result<String> {
    let mut args: Vec<String> = vec!["mr".into(), "create".into(), "--yes".into()];
    if opts.title.trim().is_empty() {
        args.push("--fill".into());
    } else {
        args.push("--title".into());
        args.push(opts.title);
        // GitLab calls the body the "description".
        args.push("--description".into());
        args.push(opts.body);
    }
    if let Some(base) = opts.base.filter(|b| !b.trim().is_empty()) {
        args.push("--target-branch".into());
        args.push(base);
    }
    if opts.draft {
        args.push("--draft".into());
    }
    let (ok, stdout, stderr) = GlabCmd::new(cwd).args(args).run_raw().await?;
    if !ok {
        return Err(GitError::NonZero {
            code: 1,
            stderr: stderr.trim().to_string(),
        });
    }
    let url = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("http"))
        .unwrap_or("")
        .to_string();
    Ok(url)
}

/// Map a [`MergeMethod`] onto `glab mr merge`'s flags. GitLab's default (no
/// flag) is a merge commit; `--squash` and `--rebase` mirror GitHub.
fn merge_flag(method: MergeMethod) -> Option<&'static str> {
    match method {
        MergeMethod::Merge => None,
        MergeMethod::Squash => Some("--squash"),
        MergeMethod::Rebase => Some("--rebase"),
    }
}

/// Merge the current branch's open MR via `glab mr merge --yes <method>`.
/// `--yes` skips the confirmation prompt; no `--remove-source-branch` is passed
/// (branch cleanup stays the user's call, matching the `gh` path).
///
/// `--auto-merge=false` forces an *immediate* merge: glab's `--auto-merge`
/// defaults to true, which would queue a merge-when-pipeline-succeeds and exit
/// 0 — the call would report success while the MR stayed open. Disabling it
/// makes glab merge now or fail loudly if the pipeline gate blocks it, matching
/// `gh pr merge`'s semantics (the error then carries glab's stderr, e.g.
/// "pipeline must succeed").
pub async fn mr_merge(cwd: impl AsRef<Path>, method: MergeMethod) -> Result<()> {
    let mut args: Vec<&str> = vec!["mr", "merge", "--yes", "--auto-merge=false"];
    if let Some(flag) = merge_flag(method) {
        args.push(flag);
    }
    let (ok, _stdout, stderr) = GlabCmd::new(cwd).args(args).run_raw().await?;
    if !ok {
        return Err(GitError::NonZero {
            code: 1,
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(())
}

/// Body + author of one issue/MR — `glab issue|mr view N -F json`. GitLab
/// spells the body `description` and the author `author.username`, mapped onto
/// the shared [`ItemDetail`]. Same lazy-on-open + graceful-`None` contract as
/// [`crate::gh::item_detail`].
pub async fn item_detail(
    cwd: impl AsRef<Path>,
    kind: oximux_core::ForgeRefKind,
    number: u64,
    repo: Option<&str>,
) -> Option<ItemDetail> {
    let subcommand = match kind {
        oximux_core::ForgeRefKind::Issue => "issue",
        oximux_core::ForgeRefKind::Pull => "mr",
    };
    let mut cmd = GlabCmd::new(cwd).args([subcommand, "view", &number.to_string(), "-F", "json"]);
    if let Some(slug) = repo {
        cmd = cmd.args(["-R", slug]);
    }
    let (ok, stdout, _) = cmd.run_raw().await.ok()?;
    if !ok {
        return None;
    }
    let gl: GitlabDetail = serde_json::from_str(stdout.trim()).ok()?;
    Some(ItemDetail {
        body: gl.description,
        author: ForgeAuthor {
            login: gl.author.username,
        },
    })
}

/// GitLab's issue/MR detail shape: `description` (not `body`) and an `author`
/// keyed `username` (not `login`). Mapped onto the shared [`ItemDetail`].
#[derive(Debug, Clone, Default, Deserialize)]
struct GitlabDetail {
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: GitlabAuthor,
}

/// GitLab spells the author's handle `username` (not GitHub's `login`); shared
/// by the detail and list shapes.
#[derive(Debug, Clone, Default, Deserialize)]
struct GitlabAuthor {
    #[serde(default)]
    username: String,
}

/// One issue/MR row as GitLab's `glab ... list -F json` reports it. GitLab's
/// shape diverges from GitHub's: `iid` (not `number`), `web_url` (not `url`),
/// labels as bare strings, assignees keyed `username`. Mapped onto the shared
/// [`ForgeItem`] by [`GitlabItem::into_forge_item`].
#[derive(Debug, Clone, Deserialize)]
struct GitlabItem {
    #[serde(default)]
    iid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    web_url: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignees: Vec<GitlabAssignee>,
    /// Issue/MR author — `Option` because GitLab can report a null author for
    /// system-generated items.
    #[serde(default)]
    author: Option<GitlabAuthor>,
    // GitLab already spells this snake_case, so it maps onto the shared
    // `ForgeItem.updated_at` without a rename.
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitlabAssignee {
    #[serde(default)]
    username: String,
}

/// Map a GitLab MR/issue `state` onto GitHub's spelling (`OPEN` / `MERGED` /
/// `CLOSED`) so shared UI keyed on the GitHub tokens treats both forges alike.
/// `locked` collapses to `CLOSED` (not mergeable, not open); an unrecognized
/// value upper-cases through so nothing is silently dropped.
fn normalize_state(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "opened" => "OPEN".to_string(),
        "merged" => "MERGED".to_string(),
        "closed" | "locked" => "CLOSED".to_string(),
        _ => raw.trim().to_ascii_uppercase(),
    }
}

impl GitlabItem {
    fn into_forge_item(self) -> ForgeItem {
        ForgeItem {
            number: self.iid,
            title: self.title,
            // Normalize GitLab's vocabulary onto GitHub's so the Tasks page's
            // state styling (which matches `OPEN`/`MERGED`) works unchanged.
            // A bare upper-case would yield `OPENED`/`LOCKED`, which that styling
            // doesn't recognize — open MRs would render in the closed color.
            state: normalize_state(&self.state),
            url: self.web_url,
            labels: self
                .labels
                .into_iter()
                .map(|name| ForgeLabel { name })
                .collect(),
            assignees: self
                .assignees
                .into_iter()
                .map(|a| ForgeAssignee { login: a.username })
                .collect(),
            author: ForgeAuthor {
                login: self.author.map(|a| a.username).unwrap_or_default(),
            },
            updated_at: self.updated_at,
        }
    }
}

/// Cap on rows fetched per listing — mirrors the `gh` page size.
const LIST_LIMIT: &str = "50";

/// List issues for the repo at `cwd` via `glab issue list -F json`. Same
/// graceful-empty contract as [`crate::gh::issue_list`].
pub async fn issue_list(cwd: impl AsRef<Path>, filter: ForgeListFilter) -> Vec<ForgeItem> {
    forge_list(cwd, "issue", filter).await
}

/// List merge requests for the repo at `cwd` via `glab mr list -F json`. Same
/// graceful-empty contract as [`crate::gh::pr_list`].
pub async fn mr_list(cwd: impl AsRef<Path>, filter: ForgeListFilter) -> Vec<ForgeItem> {
    forge_list(cwd, "mr", filter).await
}

async fn forge_list(cwd: impl AsRef<Path>, kind: &str, filter: ForgeListFilter) -> Vec<ForgeItem> {
    // GitLab spells the states `opened`/`closed`/`all`; map from the shared
    // ForgeState (whose `flag()` yields GitHub's `open`).
    let state = match filter.state {
        crate::gh::ForgeState::Open => "opened",
        crate::gh::ForgeState::Closed => "closed",
        crate::gh::ForgeState::All => "all",
    };
    let mut args: Vec<String> = vec![
        kind.into(),
        "list".into(),
        "-F".into(),
        "json".into(),
        "--per-page".into(),
        LIST_LIMIT.into(),
    ];
    // GitLab list state flags: issues/MRs accept `--opened`/`--closed`/`--all`.
    match state {
        "opened" => {} // default listing is open already
        "closed" => args.push("--closed".into()),
        "all" => args.push("--all".into()),
        _ => {}
    }
    if filter.mine {
        args.push("--assignee".into());
        args.push("@me".into());
    }
    // GitLab's API combines free-text `--search` with the state/assignee flags
    // (unlike GitHub's Search API, which ignores them), so the chips stay as
    // flags above and the query rides alongside as plain text. GitHub-style
    // qualifiers (`is:open`, `label:bug`) are not GitLab search syntax — they
    // match as literal text here. This is intentional: the query box is
    // GitHub-oriented, and translating qualifiers to GitLab filters is left out
    // deliberately rather than guessed at.
    if let Some(query) = filter.search.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        args.push("--search".into());
        args.push(query.into());
    }
    let Ok((_ok, stdout, _stderr)) = GlabCmd::new(cwd)
        .args(args)
        .timeout(Duration::from_secs(15))
        .run_raw()
        .await
    else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<GitlabItem>>(stdout.trim())
        .unwrap_or_default()
        .into_iter()
        .map(GitlabItem::into_forge_item)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mr_state_reads_gitlab_tokens() {
        assert_eq!(parse_mr_state(r#"{"state":"opened"}"#), PrState::Open);
        assert_eq!(parse_mr_state(r#"{"state":"merged"}"#), PrState::Merged);
        assert_eq!(parse_mr_state(r#"{"state":"closed"}"#), PrState::Closed);
        assert_eq!(parse_mr_state(r#"{"state":"locked"}"#), PrState::Closed);
        assert_eq!(parse_mr_state("{}"), PrState::None);
    }

    #[test]
    fn merge_flag_maps_methods() {
        assert_eq!(merge_flag(MergeMethod::Merge), None);
        assert_eq!(merge_flag(MergeMethod::Squash), Some("--squash"));
        assert_eq!(merge_flag(MergeMethod::Rebase), Some("--rebase"));
    }

    #[test]
    fn gitlab_item_maps_onto_forge_item() {
        let json = r#"[
            {"iid":42,"title":"Fix crash","state":"opened","web_url":"https://gl/42",
             "labels":["bug","p1"],"assignees":[{"username":"alice"}],
             "author":{"username":"bob"},"updated_at":"2026-06-30T10:00:00Z"},
            {"iid":7,"title":"Docs","state":"merged","web_url":"https://gl/7",
             "labels":[],"assignees":[]}
        ]"#;
        let items: Vec<ForgeItem> = serde_json::from_str::<Vec<GitlabItem>>(json)
            .unwrap()
            .into_iter()
            .map(GitlabItem::into_forge_item)
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].number, 42);
        assert_eq!(items[0].title, "Fix crash");
        // GitLab `opened` normalized to GitHub's `OPEN` so shared state styling
        // (which keys on OPEN/MERGED) renders an open MR as live, not closed.
        assert_eq!(items[0].state, "OPEN");
        assert_eq!(items[0].url, "https://gl/42");
        assert_eq!(items[0].labels.len(), 2);
        assert_eq!(items[0].labels[1].name, "p1");
        assert_eq!(items[0].assignees[0].login, "alice");
        // GitLab's `author.username` maps onto the shared `author.login`.
        assert_eq!(items[0].author.login, "bob");
        // GitLab's snake_case `updated_at` maps through with no rename.
        assert_eq!(items[0].updated_at, "2026-06-30T10:00:00Z");
        assert_eq!(items[1].number, 7);
        assert_eq!(items[1].state, "MERGED");
        assert!(items[1].assignees.is_empty());
        // A null/absent author maps to an empty login (no panic).
        assert!(items[1].author.login.is_empty());
        // Missing `updated_at` defaults to empty (column renders a dash).
        assert!(items[1].updated_at.is_empty());
    }

    #[test]
    fn normalize_state_maps_gitlab_to_github_vocab() {
        assert_eq!(normalize_state("opened"), "OPEN");
        assert_eq!(normalize_state("merged"), "MERGED");
        assert_eq!(normalize_state("closed"), "CLOSED");
        assert_eq!(normalize_state("locked"), "CLOSED");
        // Unknown passes through upper-cased rather than vanishing.
        assert_eq!(normalize_state("draft"), "DRAFT");
    }

    #[test]
    fn parse_mr_state_ignores_state_word_in_other_fields() {
        // A branch literally named "merged" must not override the real state.
        let json = r#"{"state":"opened","source_branch":"merged","title":"closed bug"}"#;
        assert_eq!(parse_mr_state(json), PrState::Open);
    }

    #[test]
    fn gitlab_item_tolerates_missing_fields() {
        let json = r#"[{"iid":3,"title":"bare"}]"#;
        let items: Vec<GitlabItem> = serde_json::from_str(json).unwrap();
        let mapped = items[0].clone().into_forge_item();
        assert_eq!(mapped.number, 3);
        assert!(mapped.url.is_empty());
        assert!(mapped.labels.is_empty());
    }

    #[tokio::test]
    async fn is_gitlab_remote_false_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_gitlab_remote(tmp.path()).await);
    }

    #[tokio::test]
    async fn has_open_mr_false_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!has_open_mr(tmp.path()).await);
    }

    #[tokio::test]
    async fn mr_list_empty_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            mr_list(tmp.path(), ForgeListFilter::default())
                .await
                .is_empty()
        );
    }
}
