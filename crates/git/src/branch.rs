//! Branch operations on `Repository`: `list_branches`, `create_branch`,
//! `switch_branch`. v1 = local branches only; remote branches are not listed
//! or created.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::BranchInfo;

/// Field separator for `git branch --format`. Picked because the tab character
/// is rejected by `git check-ref-format` from branch names, so it can't
/// collide with name/upstream content.
const SEP: &str = "\t";

impl Repository {
    /// List local branches. Order matches git's natural ordering (alphabetical).
    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        // %(HEAD) is "*" on the current branch, " " on others. We emit the
        // current flag as a non-ambiguous "1" / "0" instead.
        let format = format!(
            "%(refname:short){SEP}%(if)%(HEAD)%(then)1%(else)0%(end){SEP}%(upstream:short)"
        );
        let out = GitCmd::new(self.workdir())
            .args(["branch", "--list", "--format"])
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
}
