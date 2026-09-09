//! Worktree operations on `Repository`: `add_worktree`, `list_worktrees`,
//! `remove_worktree`. Branch convention: `oximux/<slug>` — a slug must be a
//! valid single git ref-name component (no slashes, no `..`, no whitespace,
//! no `@{`).

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::WorktreeInfo;
use std::path::{Path, PathBuf};

/// The working directory of the main repository that owns the linked worktree
/// at `dir`, or `None` when `dir` is not a linked worktree.
///
/// **Synchronous and process-free**, unlike [`Repository::open`], because the
/// caller runs on the agent-spawn path — a `git` subprocess there would block
/// the UI thread on every chat that starts. It reads exactly the files git
/// itself uses: a linked worktree's `.git` is a *file* holding
/// `gitdir: <main>/.git/worktrees/<name>`, and that directory holds a
/// `commondir` pointing back at the shared `.git`.
///
/// `commondir` rather than stripping `worktrees/<name>` off the tail: it is the
/// pointer git maintains for this purpose, and it stays correct when the shared
/// directory is somewhere the conventional layout would not predict.
///
/// Returns `None` on anything unexpected — a primary worktree, an unreadable
/// pointer, a bare repository with no working tree to attribute. Every caller
/// treats `None` as "not part of that project", so an uncertain answer withholds
/// a capability rather than granting one.
pub fn main_worktree_of(dir: &Path) -> Option<PathBuf> {
    // A primary worktree's `.git` is a directory, so this read fails and the
    // question is already answered.
    let pointer = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = pointer.trim().strip_prefix("gitdir:")?.trim();
    // Absolute in practice; joined so a relative pointer resolves against the
    // worktree, which is what git means by one.
    let gitdir = dir.join(gitdir);

    let commondir = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    // Canonicalized because `commondir` is written relative (`../..`) and
    // `Path::parent` would otherwise hand back a path still ending in `..`.
    let common = gitdir.join(commondir.trim()).canonicalize().ok()?;

    // `<main>/.git` → `<main>`. A bare repository's shared directory is named
    // for the repo (`foo.git`), and its parent is somebody else's folder, so
    // require the conventional layout rather than guessing a working tree that
    // does not exist.
    if common.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

impl Repository {
    /// Create a new linked worktree at `path` checked out on a brand-new
    /// `branch` (created from the current HEAD).
    ///
    /// **The caller names the branch.** This used to derive `oximux/<slug>`
    /// itself, which made the prefix unconfigurable from the one place that
    /// could see the user's settings — and left the dialog's preview deriving
    /// the same name a second time, free to disagree. Resolution now happens
    /// once, above, in `oximux_worktree_ops::branch_name`.
    ///
    /// `path` must not already exist; `branch` must pass
    /// [`validate_branch_name`]. The branch must not already exist (git
    /// refuses with `NonZero` if it does).
    ///
    /// **Branches from whatever HEAD happens to be.** The desktop and the CLI
    /// no longer call this — they name a base explicitly through
    /// [`add_worktree_from`](Self::add_worktree_from), because "wherever HEAD
    /// was" silently bases a new worktree on a half-finished feature branch.
    /// This form stays for callers that genuinely mean the current HEAD.
    pub async fn add_worktree(&self, path: &Path, branch: &str) -> Result<WorktreeInfo> {
        validate_branch_name(branch)?;
        GitCmd::new(self.workdir())
            .args(["worktree", "add", "-b", branch, "--"])
            .arg(path.as_os_str())
            .run()
            .await?;
        self.worktree_at(path).await
    }

    /// Create a linked worktree at `path` on a new `branch` cut from
    /// `start_point`, rather than from the main checkout's HEAD.
    ///
    /// `start_point` is anything git resolves as a commit-ish — a branch, a
    /// tag, a SHA. It must pass [`validate_ref_name`]; `branch` must pass
    /// [`validate_branch_name`] and must not already exist.
    ///
    /// Both positionals sit after a `--` terminator. Git would treat a
    /// flag-shaped ref as a reference name even without it (it fails with
    /// `invalid reference: --force`), but the terminator is what makes that a
    /// property of the argv rather than of git's current option parser, and it
    /// matches `merge.rs` and `branch.rs`.
    ///
    /// # The start point is resolved to a SHA first, and that is load-bearing
    ///
    /// `git worktree add` DWIMs a commit-ish that names no local branch but
    /// exactly one remote-tracking branch into `--track -b <that name>` — and
    /// **that override beats an explicit `-b`**. Verified on git 2.55:
    ///
    /// ```text
    /// $ git worktree add -b oximux/x -- ../wt main   # local `main` deleted,
    /// Preparing worktree (new branch 'main')          # origin/main present
    /// branch 'main' set up to track 'origin/main'.
    /// ```
    ///
    /// The worktree lands on `main`, `oximux/x` is never created, and nothing
    /// reports a problem. The `workspaces` row would then name a branch that
    /// does not exist, breaking the three-way agreement the crate is built on —
    /// and a worktree-centric user who deleted their local `main` hits it on an
    /// ordinary create with no flags at all.
    ///
    /// Passing a raw SHA removes the ambiguity DWIM keys on, so `-b` is
    /// honoured. A start point that does not resolve is refused here rather
    /// than silently reinterpreted.
    pub async fn add_worktree_from(
        &self,
        path: &Path,
        branch: &str,
        start_point: &str,
    ) -> Result<WorktreeInfo> {
        validate_branch_name(branch)?;
        validate_ref_name(start_point)?;
        let sha = self.sha_of(start_point).await?.ok_or_else(|| {
            GitError::invalid_input(format!("start point {start_point:?} does not resolve"))
        })?;
        GitCmd::new(self.workdir())
            .args(["worktree", "add", "-b", branch, "--"])
            .arg(path.as_os_str())
            .arg(&sha)
            .run()
            .await?;
        self.worktree_at(path).await
    }

    /// Check an **existing** `branch` out into a new linked worktree at `path`.
    ///
    /// No `-b`, so no branch is created and no prefix applies: the worktree
    /// adopts the branch under whatever name it already has. `branch` must pass
    /// [`validate_ref_name`] rather than [`validate_branch_name`] — an adopted
    /// branch is somebody else's name and may carry more than one prefix
    /// segment (`feature/api/retry`), which the one-segment branch rule exists
    /// to forbid only for names OxiMux itself mints.
    ///
    /// Git refuses when the branch is already checked out in another worktree,
    /// and its message names that worktree; the error carries git's own text so
    /// the caller can surface the reason verbatim.
    ///
    /// # Why this insists the local branch already exists
    ///
    /// "Adopt" has to mean adopt. Handed a name git resolves some other way,
    /// `git worktree add <path> <name>` quietly does something else — and both
    /// alternatives break a caller that has already decided this branch was not
    /// minted here:
    ///
    /// - a name that exists only as `origin/<name>` **creates** a local branch
    ///   (`Preparing worktree (new branch 'main')`), which the create path then
    ///   declines to clean up on rollback, because it believes it adopted one;
    /// - a **tag** or a raw SHA produces a detached HEAD, leaving the
    ///   `workspaces` row naming a branch the worktree is not on.
    ///
    /// So the check is `refs/heads/<branch>`, not "does this resolve".
    /// Rejecting here costs the user an explicit `--from` for the cases they
    /// probably meant; accepting silently costs them the invariant.
    pub async fn add_worktree_existing(&self, path: &Path, branch: &str) -> Result<WorktreeInfo> {
        validate_ref_name(branch)?;
        if self.sha_of(&format!("refs/heads/{branch}")).await?.is_none() {
            return Err(GitError::invalid_input(format!(
                "no local branch named {branch:?} \
                 (a remote-only branch or a tag needs `--from` instead)"
            )));
        }
        GitCmd::new(self.workdir())
            .args(["worktree", "add", "--"])
            .arg(path.as_os_str())
            .arg(branch)
            .run()
            .await?;
        self.worktree_at(path).await
    }

    /// The freshly-added worktree at `path`, looked up in `git worktree list`.
    async fn worktree_at(&self, path: &Path) -> Result<WorktreeInfo> {
        // Look up the newly-added worktree by path. BOTH sides are
        // canonicalized: the caller may have passed a relative or symlinked
        // path, and git's `--porcelain` output is not `fs::canonicalize`'s
        // shape either — on Windows git prints `C:/...` while canonicalize
        // returns the `\\?\C:\...` verbatim form, which never compares equal
        // as `PathBuf`s. Round-tripping each listed path through the same
        // canonicalization is what makes the comparison mean "same directory"
        // rather than "same spelling".
        let target = std::fs::canonicalize(path)
            .map_err(|e| GitError::parse(format!("canonicalize worktree path: {e}")))?;
        let entries = self.list_worktrees().await?;
        entries
            .into_iter()
            .find(|w| std::fs::canonicalize(&w.path).is_ok_and(|p| p == target))
            .ok_or_else(|| {
                GitError::parse(format!(
                    "new worktree at {target:?} not present in `git worktree list`"
                ))
            })
    }

    /// List all worktrees (main first, then linked).
    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let out = GitCmd::new(self.workdir())
            .args(["worktree", "list", "--porcelain"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git worktree list`: {e}")))?;
        parse_worktree_list(&text)
    }

    /// Remove a linked worktree. `force=true` passes `--force` (allows
    /// removal even with uncommitted changes inside the worktree).
    ///
    /// Refuses to remove the main worktree (returns `InvalidInput`) — git
    /// itself errors there too, but failing early avoids spawning git for a
    /// guaranteed user mistake.
    pub async fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        // Compare canonical paths to defend against symlinks / `..` indirection.
        let target = std::fs::canonicalize(path)
            .map_err(|e| GitError::parse(format!("canonicalize worktree path: {e}")))?;
        let main = std::fs::canonicalize(self.workdir())
            .map_err(|e| GitError::parse(format!("canonicalize workdir: {e}")))?;
        if target == main {
            return Err(GitError::invalid_input("cannot remove main worktree"));
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["worktree", "remove"]);
        if force {
            cmd = cmd.arg("--force");
        }
        cmd.arg(path.as_os_str()).run().await?;
        Ok(())
    }

    /// Move a linked worktree from `from` to `to`, letting git update its own
    /// `gitdir` bookkeeping.
    ///
    /// Never do this with a plain `mv`: a linked worktree's `.git` file and the
    /// `.git/worktrees/<name>/gitdir` pointer reference each other by absolute
    /// path, and moving the directory behind git's back desynchronises them
    /// into exactly the wedged state a rename is supposed to remove.
    ///
    /// Git refuses on a locked worktree and can refuse on submodules; the error
    /// carries git's own text so the caller can surface the reason verbatim
    /// rather than guessing at it.
    pub async fn move_worktree(&self, from: &Path, to: &Path) -> Result<()> {
        // Refuse the main worktree early, for the reason `remove_worktree`
        // gives: git errors too, but a guaranteed mistake need not spawn git.
        let source = std::fs::canonicalize(from)
            .map_err(|e| GitError::parse(format!("canonicalize worktree path: {e}")))?;
        let main = std::fs::canonicalize(self.workdir())
            .map_err(|e| GitError::parse(format!("canonicalize workdir: {e}")))?;
        if source == main {
            return Err(GitError::invalid_input("cannot move main worktree"));
        }
        if to.exists() {
            return Err(GitError::invalid_input(format!(
                "destination already exists: {}",
                to.display()
            )));
        }
        GitCmd::new(self.workdir())
            .args(["worktree", "move"])
            .arg(from.as_os_str())
            .arg(to.as_os_str())
            .run()
            .await?;
        Ok(())
    }
}

/// Reject slug values that would either be ambiguous as a branch component
/// or trigger `git check-ref-format` failures inside `add_worktree`.
///
/// Rules (intersection of "what git accepts" and "what's unambiguous as a
/// ref-path component" — git's own `check-ref-format` is the upstream
/// authority but we front-load the most common rejection rules to fail fast):
/// - Non-empty
/// - No slash (slug is one component; nested namespaces deliberately disallowed)
/// - No whitespace anywhere
/// - No `..` (relative-ref-path injection)
/// - No `@{` (reflog selector syntax)
/// - No `~` `^` `:` (revision modifier syntax — `oximux/feat^1` would parse
///   as a relative ref in subsequent git commands)
/// - No leading `-` (would be parsed as a flag by `git worktree add -b`)
/// - No leading or trailing `.` (rejected by `git check-ref-format`)
/// - No trailing `.lock` (collides with git lockfile naming)
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        return Err(GitError::invalid_input("slug is empty"));
    }
    if slug.starts_with('-') {
        return Err(GitError::invalid_input(
            "slug starts with '-' (would be parsed as a flag)",
        ));
    }
    if slug.starts_with('.') || slug.ends_with('.') {
        return Err(GitError::invalid_input(
            "slug starts or ends with '.' (rejected by git check-ref-format)",
        ));
    }
    if slug.ends_with(".lock") {
        return Err(GitError::invalid_input(
            "slug ends with '.lock' (collides with git lockfile naming)",
        ));
    }
    for bad in ["/", "..", "@{", "~", "^", ":"] {
        if slug.contains(bad) {
            return Err(GitError::invalid_input(format!(
                "slug contains forbidden sequence {bad:?}"
            )));
        }
    }
    if slug.chars().any(char::is_whitespace) {
        return Err(GitError::invalid_input("slug contains whitespace"));
    }
    Ok(())
}

/// Reject branch names OxiMux would not be able to hand to git safely.
///
/// [`validate_slug`] deliberately rejects `/`, because a slug is one path
/// component. A *branch name* is one optional prefix segment plus that slug —
/// `oximux/feat`, `nhtera/feat`, or a bare `feat` when the user has turned the
/// prefix off. So the resolved name cannot go through `validate_slug` at all,
/// and the check that replaces it has to allow exactly one more segment and no
/// further.
///
/// Each segment is held to `validate_slug`'s rules, which is what keeps the
/// prefix half from smuggling in the revision syntax (`~`, `^`, `:`, `@{`,
/// `..`) that the slug half is screened for. Empty segments are rejected
/// explicitly: `/feat`, `oximux/`, and `a//b` all reach git as refs it either
/// refuses or, worse, accepts as something other than what was meant.
pub fn validate_branch_name(branch: &str) -> Result<()> {
    let mut segments = branch.split('/');
    let first = segments.next().unwrap_or_default();
    let second = segments.next();
    if segments.next().is_some() {
        return Err(GitError::invalid_input(format!(
            "branch name {branch:?} has more than one prefix segment"
        )));
    }
    match second {
        // `a/b`: both halves must independently be a valid slug. The prefix is
        // checked first so its own error names the half that is wrong.
        Some(slug) => {
            validate_slug(first)?;
            validate_slug(slug)
        }
        None => validate_slug(first),
    }
}

/// Reject ref names OxiMux would not be able to hand to git safely.
///
/// A *ref* is not a branch OxiMux minted, so neither of the existing validators
/// fits. [`validate_slug`] rejects `/`, which every remote-tracking ref and most
/// real branch names contain. [`validate_branch_name`] allows exactly one `/`,
/// because that is the shape of `<prefix>/<slug>` — but a base ref the user
/// chose (`feature/api/retry`, `origin/main`, `v1.2.0`) legitimately carries
/// more, and refusing it would refuse the feature.
///
/// So this validates the whole name at once against the subset of
/// `check-ref-format` that is actually dangerous rather than merely unusual,
/// plus the shapes that are ambiguous as *arguments*:
/// - Non-empty, and no empty segment (`a//b`, `/a`, `a/`)
/// - No leading `-` — an argument git's option parser could claim. The `--`
///   terminator on every call site already covers this; the check makes it a
///   property of the value, so a future call site that forgets the terminator
///   is not a force-reset waiting to happen.
/// - No whitespace, no ASCII control characters
/// - No `..` (relative-ref-path injection), no `@{` (reflog selector)
/// - No `~` `^` `:` `?` `*` `[` `\` (revision-modifier and glob syntax git's
///   own `check-ref-format` forbids)
/// - No segment starting or ending with `.`, and no `.lock` suffix
/// - Not a bare `@`
///
/// This is deliberately a front-load, not a replacement: git's own
/// `check-ref-format` remains the authority, and anything that slips through
/// here is refused by git with its own message rather than misinterpreted.
pub fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(GitError::invalid_input("ref name is empty"));
    }
    if name.starts_with('-') {
        return Err(GitError::invalid_input(format!(
            "ref name {name:?} starts with '-' (would be parsed as a flag)"
        )));
    }
    if name == "@" {
        return Err(GitError::invalid_input("ref name is a bare '@'"));
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_ascii_control()) {
        return Err(GitError::invalid_input(format!(
            "ref name {name:?} contains whitespace or a control character"
        )));
    }
    for bad in ["..", "@{", "~", "^", ":", "?", "*", "[", "\\"] {
        if name.contains(bad) {
            return Err(GitError::invalid_input(format!(
                "ref name {name:?} contains forbidden sequence {bad:?}"
            )));
        }
    }
    if name.ends_with(".lock") {
        return Err(GitError::invalid_input(format!(
            "ref name {name:?} ends with '.lock' (collides with git lockfile naming)"
        )));
    }
    for segment in name.split('/') {
        if segment.is_empty() {
            return Err(GitError::invalid_input(format!(
                "ref name {name:?} has an empty path segment"
            )));
        }
        if segment.starts_with('.') || segment.ends_with('.') {
            return Err(GitError::invalid_input(format!(
                "ref name {name:?} has a segment starting or ending with '.'"
            )));
        }
        if segment.ends_with(".lock") {
            return Err(GitError::invalid_input(format!(
                "ref name {name:?} has a segment ending with '.lock'"
            )));
        }
    }
    Ok(())
}

/// Derive a slug from a human-readable workspace name.
///
/// Rules: lowercase ASCII, replace each non-`[a-z0-9]` byte with `-`,
/// collapse consecutive `-` runs to a single `-`, trim leading and
/// trailing `-`, and fall back to `"workspace"` if the result is empty.
///
/// The derived slug still must be validated with [`validate_slug`]
/// before use as a branch component — `derive_slug` only normalises
/// shape; it does not guarantee the result passes every git
/// `check-ref-format` rule (for example, a slug like `"workspace.lock"`
/// could in theory be reached if a future caller built the name from a
/// trusted string).
pub fn derive_slug(name: &str) -> String {
    const FALLBACK: &str = "workspace";
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for byte in name.as_bytes() {
        let lower = byte.to_ascii_lowercase();
        let ok = lower.is_ascii_lowercase() || lower.is_ascii_digit();
        if ok {
            out.push(lower as char);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return FALLBACK.to_string();
    }
    cap_slug_len(trimmed)
}

/// Maximum derived-slug length. A slug becomes a branch component, a worktree
/// directory name, AND the string the user types to confirm deletion — so an
/// unbounded slug from a long issue title makes all three unwieldy. Cut to this
/// many bytes at a word (`-`) boundary.
const MAX_SLUG_LEN: usize = 48;

/// Trim a normalized slug to [`MAX_SLUG_LEN`], breaking on the last `-` within
/// the budget so it ends on a whole word. The input is already `[a-z0-9-]`
/// (ASCII), so byte slicing is char-boundary-safe. Falls back to a hard cut
/// when there is no dash to break on (one very long word).
fn cap_slug_len(slug: &str) -> String {
    if slug.len() <= MAX_SLUG_LEN {
        return slug.to_string();
    }
    let head = &slug[..MAX_SLUG_LEN];
    let cut = head.rfind('-').unwrap_or(MAX_SLUG_LEN);
    slug[..cut].trim_end_matches('-').to_string()
}

/// Parse `git worktree list --porcelain` output. Blocks are delimited by
/// blank lines. Each block starts with `worktree <path>` and contains
/// `HEAD <sha>`, then either `branch refs/heads/<name>` or `detached`,
/// then optionally `locked` (with an optional reason on the same line).
pub(crate) fn parse_worktree_list(text: &str) -> Result<Vec<WorktreeInfo>> {
    let mut out = Vec::new();
    let mut current: Option<WtBuilder> = None;
    let mut first = true;

    let flush = |w: &mut Option<WtBuilder>, out: &mut Vec<WorktreeInfo>, first: &mut bool| {
        if let Some(b) = w.take() {
            let info = b.into_info(*first);
            *first = false;
            out.push(info);
        }
    };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut current, &mut out, &mut first);
            continue;
        }
        let (key, rest) = match line.split_once(' ') {
            Some((k, r)) => (k, r),
            None => (line, ""),
        };
        match key {
            "worktree" => {
                // A new block — flush any in-progress one (handles files that
                // don't end with a blank line).
                flush(&mut current, &mut out, &mut first);
                current = Some(WtBuilder::new(PathBuf::from(rest)));
            }
            "HEAD" => {
                if let Some(b) = current.as_mut() {
                    b.head = rest.to_string();
                }
            }
            "branch" => {
                if let Some(b) = current.as_mut() {
                    b.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
                }
            }
            "detached" => {
                // Already None by default; nothing to do.
            }
            "locked" => {
                if let Some(b) = current.as_mut() {
                    b.is_locked = true;
                }
            }
            // bare, prunable, … — informational fields we don't surface in v1.
            _ => {}
        }
    }
    flush(&mut current, &mut out, &mut first);
    Ok(out)
}

struct WtBuilder {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    is_locked: bool,
}

impl WtBuilder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            head: String::new(),
            branch: None,
            is_locked: false,
        }
    }
    fn into_info(self, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: self.path,
            head: self.head,
            branch: self.branch,
            is_main,
            is_locked: self.is_locked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_typical_names() {
        for ok in ["feature", "fix-bug", "v1.2.3", "user_42", "ascii.dot"] {
            validate_slug(ok).unwrap_or_else(|e| panic!("{ok:?} should pass: {e:?}"));
        }
    }

    #[test]
    fn slug_rejects_empty() {
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn slug_rejects_slashes_dotdot_atbrace() {
        for bad in ["foo/bar", "..", "feat..", "head@{0}", "x@{"] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn slug_rejects_revision_modifiers_and_dot_edge_cases() {
        for bad in [
            "feat^1",
            "v1~2",
            "host:port",
            ".hidden",
            "trail.",
            "session.lock",
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn slug_rejects_whitespace() {
        for bad in ["has space", "tab\there", "trail "] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn slug_rejects_leading_dash() {
        assert!(validate_slug("-evil").is_err());
    }

    #[test]
    fn derive_slug_basic() {
        assert_eq!(derive_slug("My Feature"), "my-feature");
    }

    #[test]
    fn derive_slug_whitespace_collapse() {
        assert_eq!(derive_slug("  hello   world  "), "hello-world");
    }

    #[test]
    fn derive_slug_punctuation() {
        assert_eq!(derive_slug("feat!@#$end"), "feat-end");
    }

    #[test]
    fn derive_slug_non_ascii_replaced() {
        // Each non-ASCII byte becomes a `-`; multi-byte UTF-8 sequences
        // collapse to a single dash via the run-collapse rule.
        assert_eq!(derive_slug("héllo"), "h-llo");
    }

    #[test]
    fn derive_slug_all_rejected_fallback() {
        assert_eq!(derive_slug("!!!"), "workspace");
    }

    #[test]
    fn derive_slug_run_collapse() {
        assert_eq!(derive_slug("foo---bar"), "foo-bar");
    }

    #[test]
    fn derive_slug_caps_long_names_at_word_boundary() {
        // A sentence-length issue title must not produce a 100+ char slug.
        let long = "issue 1556 iOS Objective-C/Swift mixed-language: many expected edges missing self imports";
        let slug = derive_slug(long);
        assert!(slug.len() <= 48, "slug too long: {} ({})", slug, slug.len());
        // Ends on a whole word (no trailing dash, no mid-word cut).
        assert!(!slug.ends_with('-'));
        assert!(slug.starts_with("issue-1556-ios-objective-c-swift"));
        // A name already within budget is returned unchanged.
        assert_eq!(derive_slug("short feature"), "short-feature");
    }

    #[test]
    fn derive_slug_caps_single_long_word_hard() {
        // No dash to break on within the budget → hard cut, still valid.
        let slug = derive_slug(&"a".repeat(80));
        assert_eq!(slug.len(), 48);
        assert!(validate_slug(&slug).is_ok());
    }

    #[test]
    fn parse_main_only() {
        let text = "\
worktree /tmp/repo
HEAD abc123
branch refs/heads/main
";
        let ws = parse_worktree_list(text).unwrap();
        assert_eq!(ws.len(), 1);
        assert!(ws[0].is_main);
        assert_eq!(ws[0].head, "abc123");
        assert_eq!(ws[0].branch.as_deref(), Some("main"));
        assert!(!ws[0].is_locked);
    }

    #[test]
    fn parse_main_plus_linked() {
        let text = "\
worktree /tmp/repo
HEAD abc123
branch refs/heads/main

worktree /tmp/wt-feat
HEAD def456
branch refs/heads/oximux/feat

worktree /tmp/wt-detached
HEAD 789xyz
detached
";
        let ws = parse_worktree_list(text).unwrap();
        assert_eq!(ws.len(), 3);
        assert!(ws[0].is_main);
        assert!(!ws[1].is_main);
        assert_eq!(ws[1].branch.as_deref(), Some("oximux/feat"));
        assert_eq!(ws[2].branch, None, "detached has no branch");
    }

    #[test]
    fn parse_locked_flag() {
        let text = "\
worktree /tmp/repo
HEAD abc
branch refs/heads/main

worktree /tmp/wt
HEAD def
branch refs/heads/oximux/work
locked
";
        let ws = parse_worktree_list(text).unwrap();
        assert!(ws[1].is_locked);
    }
}
