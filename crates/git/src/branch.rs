//! Branch operations on `Repository`: `list_branches`, `create_branch`,
//! `switch_branch`, `delete_branch`. Local-only.
//!
//! Remote-tracking ref enumeration (`origin/main`, etc.) lives next to
//! the network ops in [`crate::remote::Repository::list_remote_branches`]
//! — same `BranchInfo` shape, separate cache, no fetch side effect.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::BranchInfo;

/// Field separator for `git branch --format`. Picked because the tab character
/// is rejected by `git check-ref-format` from branch names, so it can't
/// collide with name/upstream content.
const SEP: &str = "\t";

impl Repository {
    /// List local branches, most-recently-committed first — the branch the
    /// user just worked on surfaces at the top of the switch picker instead
    /// of wherever the alphabet put it.
    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        // %(HEAD) is "*" on the current branch, " " on others. We emit the
        // current flag as a non-ambiguous "1" / "0" instead.
        let format = format!(
            "%(refname:short){SEP}%(if)%(HEAD)%(then)1%(else)0%(end){SEP}%(upstream:short)"
        );
        let out = GitCmd::new(self.workdir())
            .args(["branch", "--list", "--sort=-committerdate", "--format"])
            .arg(format)
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git branch --list`: {e}")))?;
        parse_branch_list(&text)
    }

    /// Create a new local branch. `from` may be any ref-ish (commit SHA,
    /// branch name, tag); defaults to HEAD.
    ///
    /// Does NOT switch to the new branch.
    pub async fn create_branch(&self, name: &str, from: Option<&str>) -> Result<()> {
        if name.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["branch", "--", name]);
        if let Some(start) = from {
            cmd = cmd.arg(start);
        }
        cmd.run().await?;
        Ok(())
    }

    /// Switch the working tree to `name`. Requires a clean tree — git refuses
    /// with NonZero if the switch would discard local changes. This method
    /// does NOT auto-stash; that orchestration belongs in `merge.rs` (the only
    /// place we want auto-stash behavior in v1).
    pub async fn switch_branch(&self, name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        GitCmd::new(self.workdir())
            .args(["switch", "--", name])
            .run()
            .await?;
        Ok(())
    }

    /// Delete a local branch. `force=false` uses `git branch -d` (refuses
    /// unmerged); `force=true` uses `-D` (destructive — discards work).
    /// The workspace delete flow always passes `force=false` so the user
    /// sees the "has unmerged changes" error rather than silently losing
    /// commits; a future force-delete affordance can flip the flag.
    pub async fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        if name.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        let flag = if force { "-D" } else { "-d" };
        GitCmd::new(self.workdir())
            .args(["branch", flag, "--", name])
            .run()
            .await?;
        Ok(())
    }

    /// Rename local branch `old` to `new`, via `git branch -m`.
    ///
    /// Cheap and locally reversible — renaming back restores the previous state
    /// exactly — which is why a rollback-bearing caller does this AFTER the
    /// worktree move, the step that can half-fail.
    ///
    /// Git refuses when `new` already exists (no `--force` is offered here on
    /// purpose: silently overwriting another branch is never what a rename
    /// meant). Renaming a branch that has been pushed orphans its remote ref;
    /// that judgement belongs to the caller, so check
    /// [`upstream_of`](Self::upstream_of) first.
    pub async fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        if old.is_empty() || new.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        GitCmd::new(self.workdir())
            .args(["branch", "-m", "--", old, new])
            .run()
            .await?;
        Ok(())
    }

    /// The upstream ref `branch` tracks (e.g. `origin/feat`), or `None` when it
    /// tracks nothing.
    ///
    /// `None` is the only state in which renaming the branch is safe: renaming
    /// a pushed branch leaves the remote ref behind and breaks any open PR that
    /// points at it. A branch pushed to a remote that has since been removed
    /// reads as `None` and will be renamed — accepted, because its remote ref
    /// is already orphaned.
    ///
    /// A non-zero exit means "no upstream configured", which is a normal
    /// answer here rather than a failure, so it maps to `Ok(None)`.
    pub async fn upstream_of(&self, branch: &str) -> Result<Option<String>> {
        if branch.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        let out = GitCmd::new(self.workdir())
            .args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                &format!("{branch}@{{upstream}}"),
            ])
            .run()
            .await;
        match out {
            Ok(out) => {
                let text = String::from_utf8(out.stdout).map_err(|e| {
                    GitError::parse(format!("non-utf8 in `git rev-parse @{{upstream}}`: {e}"))
                })?;
                let upstream = text.trim();
                Ok((!upstream.is_empty()).then(|| upstream.to_string()))
            }
            // `git rev-parse` exits non-zero when no upstream is configured.
            // Anything else (a broken repo, git missing) is still an error.
            Err(GitError::NonZero { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// The repository's default branch — the one work is meant to land in.
    ///
    /// Order, best evidence first:
    ///
    /// 1. `refs/remotes/origin/HEAD`, which the remote itself declares. Read
    ///    with `symbolic-ref` and stripped of its remote prefix, so `origin/dev`
    ///    answers `dev`.
    /// 2. A local branch called `main`, then `master` — the two conventions,
    ///    checked in that order and only if they actually exist.
    ///
    /// `None` when none of those resolve (a bare local repo on some other
    /// name). Callers fall back to whatever they already had rather than
    /// guessing: naming a branch that does not exist produces a refusal the
    /// user cannot act on, which is worse than not offering the action.
    ///
    /// Not `current_branch`: where HEAD happens to be is what a merge
    /// pre-flight *compares against*, so using it as the target would make that
    /// comparison vacuous.
    pub async fn default_branch(&self) -> Result<Option<String>> {
        if let Ok(out) = GitCmd::new(self.workdir())
            .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .run()
            .await
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = text.strip_prefix("origin/")
                && !name.is_empty()
            {
                return Ok(Some(name.to_string()));
            }
        }
        for cand in ["main", "master"] {
            let raw = GitCmd::new(self.workdir())
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{cand}"),
                ])
                .run_raw()
                .await?;
            if raw.status.success() {
                return Ok(Some(cand.to_string()));
            }
        }
        Ok(None)
    }

    /// Resolve any revision to a commit sha; `None` when it does not exist.
    ///
    /// Absence is an ordinary answer here, not an error: callers ask about
    /// refs that legitimately may not be there — a local `main` in a repo
    /// that only has `master`, an `origin/main` in a repo with no remote.
    /// `--verify --quiet` makes git exit 1 for those instead of printing a
    /// diagnostic.
    pub async fn sha_of(&self, rev: &str) -> Result<Option<String>> {
        let raw = GitCmd::new(self.workdir())
            .args(["rev-parse", "--verify", "--quiet", rev])
            .run_raw()
            .await?;
        if !raw.status.success() {
            return Ok(None);
        }
        let sha = String::from_utf8_lossy(&raw.stdout).trim().to_string();
        Ok((!sha.is_empty()).then_some(sha))
    }

    /// `git merge --ff-only <rev>` in this checkout.
    ///
    /// Fast-forward or nothing: git refuses rather than creating a merge
    /// commit, which is what makes this safe to run unattended on a branch
    /// the user is sitting on. Never `--force`, never a plain merge.
    pub async fn fast_forward_to(&self, rev: &str) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["merge", "--ff-only", rev])
            .run()
            .await?;
        Ok(())
    }

    /// Fast-forward a local branch that is **not** checked out, by fetching
    /// the remote branch straight onto it.
    ///
    /// `git fetch <remote> <branch>:<branch>` updates the local ref only when
    /// the move is a fast-forward, and refuses outright when that branch is
    /// checked out in any worktree. Both refusals are git's, not ours — which
    /// is the point: the safety check lives where it cannot be got wrong.
    pub async fn fetch_branch_fast_forward(&self, remote: &str, branch: &str) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["fetch", remote, &format!("{branch}:{branch}")])
            .timeout(std::time::Duration::from_secs(60))
            .run()
            .await?;
        Ok(())
    }

    /// The branch HEAD is on, or `None` when HEAD is detached.
    ///
    /// One `git rev-parse --abbrev-ref HEAD`, deliberately cheaper than
    /// [`status`](Self::status): callers that only need to answer "is this
    /// checkout on the branch I expect?" should not pay for a full porcelain
    /// v2 parse plus its ahead/behind and branch-diff enrichment.
    ///
    /// A detached HEAD prints the literal `HEAD`, which is not a branch name,
    /// so it maps to `None` rather than being handed back as one.
    pub async fn current_branch(&self) -> Result<Option<String>> {
        let out = GitCmd::new(self.workdir())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git rev-parse HEAD`: {e}")))?;
        let name = text.trim();
        Ok((!name.is_empty() && name != "HEAD").then(|| name.to_string()))
    }

    /// Most-recently-visited local branches in MRU order, capped at `limit`.
    ///
    /// Parses HEAD's reflog (`git reflog show --pretty=%gs HEAD`) for
    /// `checkout: moving from <X> to <Y>` entries, taking the destination
    /// (`Y`) of each — that's the branch the user actually landed on.
    /// Deduplicates so the same branch only appears once even if it was
    /// visited multiple times. The current branch is always present in
    /// position 0 of `list_branches`; this list deliberately includes it
    /// (the most recent reflog entry IS the current branch) so callers
    /// can render a "Recent" section without filtering separately.
    ///
    /// Empty reflog (fresh clone with no checkouts yet) returns an empty
    /// vec. The command itself runs without timeout extension — reflog
    /// reads are local and bounded.
    pub async fn list_recent_branches(&self, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let out = GitCmd::new(self.workdir())
            .args(["reflog", "show", "--pretty=%gs", "HEAD"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git reflog show`: {e}")))?;
        Ok(parse_recent_branches(&text, limit))
    }
}

/// Parse the output of our `git branch --list --format=...` invocation.
/// Each non-empty line is `<name>\t<is_current 0|1>\t<upstream-or-empty>`.
/// Detached HEAD shows up as `(HEAD detached at <sha>)` — we filter it out so
/// `list_branches()` never returns "fake" branches.
pub(crate) fn parse_branch_list(text: &str) -> Result<Vec<BranchInfo>> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches(['\r', ' ']);
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(SEP);
        let name = parts.next().unwrap_or("");
        let flag = parts.next().unwrap_or("");
        let upstream = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(GitError::parse(format!(
                "branch list line {lineno}: too many fields in {line:?}"
            )));
        }
        // Detached-HEAD pseudo-entry: skip — it's not a real branch.
        if name.starts_with("(HEAD detached") {
            continue;
        }
        let is_current = match flag {
            "1" => true,
            "0" => false,
            other => {
                return Err(GitError::parse(format!(
                    "branch list line {lineno}: bad HEAD flag {other:?}"
                )));
            }
        };
        let upstream = if upstream.is_empty() {
            None
        } else {
            Some(upstream.to_string())
        };
        out.push(BranchInfo {
            name: name.to_string(),
            is_current,
            upstream,
        });
    }
    Ok(out)
}

/// Extract destination branches from `git reflog show --pretty=%gs HEAD`
/// output. Each line is a reflog subject; checkout entries look like
/// `checkout: moving from <X> to <Y>`. We take `<Y>` and dedupe, capping
/// at `limit`. Non-checkout entries (commits, resets, merges) are skipped.
///
/// Entries that move to a 40-char SHA (e.g. `git checkout abc1234...`)
/// represent detached-HEAD visits and are filtered out — they aren't
/// branch names the user would want to switch back to from a picker.
pub(crate) fn parse_recent_branches(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::<String>::new();
    let mut out = Vec::<String>::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(dest) = parse_checkout_destination(line) else {
            continue;
        };
        if looks_like_sha(dest) {
            continue;
        }
        if seen.insert(dest.to_string()) {
            out.push(dest.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Return the destination branch of a `checkout: moving from X to Y`
/// reflog subject; `None` if the line doesn't match that shape.
fn parse_checkout_destination(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("checkout: moving from ")?;
    // Split on the LAST " to " — branch names can technically contain
    // " to " (e.g. a feature branch named `migrate-to-typescript`),
    // and the source side is what appears first.
    let (_from, to) = rest.rsplit_once(" to ")?;
    let dest = to.trim();
    if dest.is_empty() { None } else { Some(dest) }
}

/// True when `s` looks like a git SHA (hex string of length 7..=40).
/// Reflog destinations that match this are detached-HEAD checkouts, not
/// branch switches, and shouldn't appear in the recent-branches list.
fn looks_like_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_main_branch() {
        let text = "main\t1\t\n";
        let bs = parse_branch_list(text).unwrap();
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].name, "main");
        assert!(bs[0].is_current);
        assert_eq!(bs[0].upstream, None);
    }

    #[test]
    fn parse_with_upstream() {
        let text = "main\t1\torigin/main\nfeat\t0\t\n";
        let bs = parse_branch_list(text).unwrap();
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(bs[1].name, "feat");
        assert!(!bs[1].is_current);
    }

    #[test]
    fn parse_filters_detached_head() {
        let text = "(HEAD detached at abc1234)\t1\t\nmain\t0\t\n";
        let bs = parse_branch_list(text).unwrap();
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].name, "main");
        assert!(!bs[0].is_current, "no real branch is current");
    }

    #[test]
    fn parse_rejects_garbage_head_flag() {
        let text = "main\tX\t\n";
        assert!(parse_branch_list(text).is_err());
    }

    #[test]
    fn parse_rejects_extra_fields() {
        let text = "main\t1\t\textra\n";
        assert!(parse_branch_list(text).is_err());
    }

    #[test]
    fn recent_empty_reflog_returns_empty() {
        assert!(parse_recent_branches("", 10).is_empty());
    }

    #[test]
    fn recent_zero_limit_returns_empty() {
        let text = "checkout: moving from main to feat\n";
        assert!(parse_recent_branches(text, 0).is_empty());
    }

    #[test]
    fn recent_extracts_destination_branches() {
        // `git reflog show HEAD` emits newest-first; the input below
        // reflects the user's most recent checkout sequence:
        //   …earlier… → feat-b → main (newest)
        let text = "checkout: moving from feat-b to main\n\
                    commit: typo fix\n\
                    checkout: moving from feat-a to feat-b\n\
                    checkout: moving from main to feat-a\n";
        let r = parse_recent_branches(text, 10);
        assert_eq!(r, vec!["main", "feat-b", "feat-a"]);
    }

    #[test]
    fn recent_dedup_preserves_most_recent_position() {
        // User toggled main↔feat several times; reflog newest-first:
        //   feat→main, main→feat, feat→main (oldest).
        // After dedup: main (first seen at the newest entry), then feat.
        let text = "checkout: moving from feat to main\n\
                    checkout: moving from main to feat\n\
                    checkout: moving from feat to main\n";
        let r = parse_recent_branches(text, 10);
        assert_eq!(r, vec!["main", "feat"]);
    }

    #[test]
    fn recent_caps_at_limit() {
        // Newest-first input; limit=2 → keep the two most recent distinct.
        let text = "checkout: moving from c to d\n\
                    checkout: moving from b to c\n\
                    checkout: moving from a to b\n\
                    checkout: moving from x to a\n";
        let r = parse_recent_branches(text, 2);
        assert_eq!(r, vec!["d", "c"]);
    }

    #[test]
    fn recent_skips_detached_head_checkouts() {
        let text = "checkout: moving from abc1234 to feat\n\
                    checkout: moving from main to abc1234def5678aabb\n";
        let r = parse_recent_branches(text, 10);
        // The 40-hex destination dropped; `abc1234` (7 hex) is only a
        // source and never enters the result list. Only `feat` survives.
        assert_eq!(r, vec!["feat"]);
    }

    #[test]
    fn recent_handles_branch_name_containing_to() {
        // Branch name "migrate-to-typescript" — split on LAST " to ".
        let text = "checkout: moving from main to migrate-to-typescript\n";
        let r = parse_recent_branches(text, 10);
        assert_eq!(r, vec!["migrate-to-typescript"]);
    }

    #[test]
    fn recent_ignores_non_checkout_entries() {
        let text = "commit: add feature\n\
                    reset: moving to HEAD~3\n\
                    merge feat: Merge made by 'ort'\n\
                    pull: Fast-forward\n";
        assert!(parse_recent_branches(text, 10).is_empty());
    }
}
