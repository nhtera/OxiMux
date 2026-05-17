//! Stash operations on `Repository`: `is_dirty`, `stash_push`, `stash_list`,
//! `stash_apply`, `stash_pop`, `stash_drop`. Thin wrappers over `git stash` +
//! `git status --porcelain` (for `is_dirty`). All ops are async.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::{StashEntry, StashRef};

impl Repository {
    /// Working tree or index has any tracked changes (untracked files do NOT
    /// count — that mirrors `git stash push` default behavior, so callers can
    /// chain `is_dirty()` → `stash_push(_, false)` without surprises).
    ///
    /// Uses porcelain v1 (`--untracked-files=no`) rather than reusing
    /// `self.status()` because we only need an empty/non-empty signal — v1 is
    /// one line per file and parses faster than the full v2 record stream.
    pub async fn is_dirty(&self) -> Result<bool> {
        let out = GitCmd::new(self.workdir())
            .args(["status", "--porcelain", "--untracked-files=no"])
            .run()
            .await?;
        Ok(!out.stdout.is_empty())
    }

    /// Push a new stash entry. Returns the freshly minted ref (`stash@{0}`).
    ///
    /// `include_untracked = true` adds `-u` (stashes untracked files too).
    /// Returns `Err(InvalidInput)` when there is nothing to stash — git itself
    /// exits 0 in this case, so we detect via stdout text rather than exit code.
    pub async fn stash_push(&self, msg: Option<&str>, include_untracked: bool) -> Result<StashRef> {
        let mut cmd = GitCmd::new(self.workdir()).args(["stash", "push"]);
        if include_untracked {
            cmd = cmd.arg("-u");
        }
        if let Some(m) = msg {
            cmd = cmd.args(["-m", m]);
        }
        let out = cmd.run().await?;
        // `git stash push` on a clean tree exits 0 with stdout "No local changes
        // to save\n" — that's a no-op, not a stash, so surface as InvalidInput.
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("No local changes to save") {
            return Err(GitError::invalid_input("nothing to stash"));
        }
        // SAFETY (v1 single-user): `git stash push` always lands the new entry
        // at `stash@{0}`. We don't round-trip through `stash list --format=%gd`
        // to verify because v1 is single-user (OxiMux + the user's terminal)
        // and no concurrent stash op can race between push-return and the
        // caller using this ref. See `StashRef::index` doc for the broader
        // index-drift caveat that holds across all v1 stash operations.
        Ok(StashRef { index: 0 })
    }

    /// List all stash entries, most-recent first (index 0 = top of stack).
    ///
    /// Returns an empty Vec when the stash stack is empty (git's stdout is
    /// empty in that case).
    pub async fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let out = GitCmd::new(self.workdir())
            .args(["stash", "list"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git stash list`: {e}")))?;
        parse_stash_list(&text)
    }

    /// Apply stash without removing it from the stack.
    pub async fn stash_apply(&self, stash_ref: &StashRef) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["stash", "apply", "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        Ok(())
    }

    /// Apply stash and remove from stack.
    pub async fn stash_pop(&self, stash_ref: &StashRef) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["stash", "pop", "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        Ok(())
    }

    /// Remove a stash entry without applying it.
    pub async fn stash_drop(&self, stash_ref: &StashRef) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["stash", "drop", "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        Ok(())
    }
}

/// Parse `git stash list` output. Each line is:
/// `stash@{N}: <branch-line>: <subject>` where `<branch-line>` is one of
/// `On <branch>` (default WIP) or `WIP on <branch>` (custom message; git
/// repeats `On <branch>` after `WIP`) or `On <branch>` literal (custom -m).
///
/// We're forgiving: split on `: ` with a hard cap of 3 chunks and treat the
/// last chunk as the message. The branch comes from the prefix `On <X>` or
/// `WIP on <X>` of the middle chunk.
pub(crate) fn parse_stash_list(text: &str) -> Result<Vec<StashEntry>> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, ": ");
        let head = parts.next().unwrap_or("");
        let middle = parts.next();
        let message = parts.next().unwrap_or("").to_string();

        let index = head
            .strip_prefix("stash@{")
            .and_then(|s| s.strip_suffix('}'))
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| {
                GitError::parse(format!(
                    "stash list line {lineno}: cannot parse ref {head:?}"
                ))
            })?;

        let middle = middle.ok_or_else(|| {
            GitError::parse(format!(
                "stash list line {lineno}: missing branch field in {line:?}"
            ))
        })?;
        let branch = middle
            .strip_prefix("WIP on ")
            .or_else(|| middle.strip_prefix("On "))
            .unwrap_or(middle)
            .to_string();

        out.push(StashEntry {
            stash_ref: StashRef { index },
            branch,
            message,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_list() {
        assert_eq!(parse_stash_list("").unwrap().len(), 0);
        assert_eq!(parse_stash_list("\n\n").unwrap().len(), 0);
    }

    #[test]
    fn parse_default_wip_messages() {
        let text = "\
stash@{0}: WIP on main: abc1234 commit subject
stash@{1}: WIP on feature: def5678 another subject
";
        let entries = parse_stash_list(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].stash_ref.index, 0);
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "abc1234 commit subject");
        assert_eq!(entries[1].stash_ref.index, 1);
        assert_eq!(entries[1].branch, "feature");
    }

    #[test]
    fn parse_custom_message() {
        let text = "stash@{0}: On main: my custom message\n";
        let entries = parse_stash_list(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "my custom message");
    }

    #[test]
    fn parse_message_with_colons() {
        // splitn(3, ": ") keeps trailing colons in the message untouched.
        let text = "stash@{0}: On main: feat(api): handle nested colons\n";
        let entries = parse_stash_list(text).unwrap();
        assert_eq!(entries[0].message, "feat(api): handle nested colons");
    }

    #[test]
    fn parse_rejects_bad_ref() {
        let text = "garbage@{0}: On main: x\n";
        assert!(parse_stash_list(text).is_err());
    }

    #[test]
    fn stash_ref_renders_braced() {
        assert_eq!(StashRef { index: 0 }.ref_string(), "stash@{0}");
        assert_eq!(StashRef { index: 12 }.ref_string(), "stash@{12}");
    }
}
