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
    /// branch `oximux/<slug>` (created from the current HEAD).
    ///
    /// `path` must not already exist; `slug` must pass [`validate_slug`].
    /// The corresponding branch must not already exist (git refuses with
    /// `NonZero` if it does).
    pub async fn add_worktree(&self, path: &Path, slug: &str) -> Result<WorktreeInfo> {
        validate_slug(slug)?;
        let branch = format!("oximux/{slug}");
        GitCmd::new(self.workdir())
            .args(["worktree", "add", "-b", &branch])
            .arg(path.as_os_str())
            .run()
            .await?;
        // Look up the newly-added worktree by path. Canonicalize because git
        // emits canonical paths in `--porcelain` output but the caller may
        // have passed e.g. a relative or symlinked path.
        let target = std::fs::canonicalize(path)
            .map_err(|e| GitError::parse(format!("canonicalize worktree path: {e}")))?;
        let entries = self.list_worktrees().await?;
        entries
            .into_iter()
            .find(|w| w.path == target)
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
