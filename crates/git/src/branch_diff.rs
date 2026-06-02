//! "Committed on Branch" support — the files a branch has changed in its
//! own (typically unpushed) commits, relative to where it diverged from
//! its base.
//!
//! Mirrors the GitLens / reference-editor "Committed on Branch" section:
//! the aggregate net change of `merge-base(base, HEAD)..HEAD`, listed like
//! status rows (one status code + `+added −removed` per file). It is a
//! READ-ONLY view — staging makes no sense against already-committed work,
//! so rows open a range diff rather than offering stage/unstage/discard.
//!
//! Base resolution (see [`Repository::resolve_branch_base`]): the current
//! branch's upstream when set, else the remote's default branch, else
//! `origin/main` / `origin/master`. Returns no base (empty section) when
//! none resolve — a purely local branch with no remote has nothing to
//! compare against.

use crate::numstat::diff_numstat_range;
use crate::process::GitCmd;
use crate::repository::Repository;
use crate::error::Result;
use oximux_core::{BranchCommittedFile, BranchRange, DiffStatus};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

impl Repository {
    /// Resolve the ref the current branch's committed work is measured
    /// against. Order: `@{upstream}` → `origin/HEAD` → `origin/main` →
    /// `origin/master`. `None` when nothing resolves (local-only branch).
    async fn resolve_branch_base(&self) -> Option<String> {
        if let Ok(raw) = GitCmd::new(self.workdir())
            .args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ])
            .run_raw()
            .await
            && raw.status.success()
        {
            let s = String::from_utf8_lossy(&raw.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        for cand in ["origin/HEAD", "origin/main", "origin/master"] {
            if let Ok(raw) = GitCmd::new(self.workdir())
                .args(["rev-parse", "--verify", "--quiet", cand])
                .run_raw()
                .await
                && raw.status.success()
            {
                return Some(cand.to_string());
            }
        }
        None
    }

    /// Compute the "Committed on Branch" file list — the net change across
    /// `merge-base(base, HEAD)..HEAD`. Returns the resolved range (for
    /// opening per-file diffs) plus one entry per file. Empty list (with
    /// `Some(range)`) when the branch is level with its base; `(None, [])`
    /// when no base resolves.
    ///
    /// Best-effort, like the numstat enrichment: any git failure degrades
    /// to an empty result rather than poisoning the surrounding status
    /// poll.
    pub async fn branch_committed(
        &self,
    ) -> Result<(Option<BranchRange>, Vec<BranchCommittedFile>)> {
        let Some(base_ref) = self.resolve_branch_base().await else {
            return Ok((None, Vec::new()));
        };

        let mb = GitCmd::new(self.workdir())
            .args(["merge-base", &base_ref, "HEAD"])
            .run_raw()
            .await?;
        if !mb.status.success() {
            return Ok((None, Vec::new()));
        }
        let merge_base = String::from_utf8_lossy(&mb.stdout).trim().to_string();
        if merge_base.is_empty() {
            return Ok((None, Vec::new()));
        }

        let head_raw = GitCmd::new(self.workdir())
            .args(["rev-parse", "HEAD"])
            .run_raw()
            .await?;
        if !head_raw.status.success() {
            return Ok((None, Vec::new()));
        }
        let head = String::from_utf8_lossy(&head_raw.stdout).trim().to_string();

        let range = BranchRange {
            base_ref,
            merge_base: merge_base.clone(),
            head: head.clone(),
        };

        // Branch tip is the merge base → no commits ahead of base.
        if head == merge_base {
            return Ok((Some(range), Vec::new()));
        }

        // `-z` for NUL-delimited, quoting-free paths; `-M -C` so renames /
        // copies report as such instead of add+delete pairs.
        let ns = GitCmd::new(self.workdir())
            .args([
                "diff",
                "--name-status",
                "-M",
                "-C",
                "-z",
                &merge_base,
                &head,
            ])
            .run_raw()
            .await?;
        if !ns.status.success() {
            return Ok((Some(range), Vec::new()));
        }
        let entries = parse_name_status_z(&ns.stdout);

        let counts = diff_numstat_range(self.workdir(), &merge_base, &head)
            .await
            .unwrap_or_default();

        let files = entries
            .into_iter()
            .map(|(status, path)| {
                let (added, removed) = counts.get(&path).copied().unwrap_or((0, 0));
                BranchCommittedFile {
                    path,
                    status,
                    added,
                    removed,
                }
            })
            .collect();
        Ok((Some(range), files))
    }
}

/// Parse `git diff --name-status -M -C -z` output into `(status, path)`
/// pairs. Record shapes:
///   - `<X>\0<path>\0`                  for A / M / D / T
///   - `R<score>\0<old>\0<new>\0`       for rename (path = new, origin = old)
///   - `C<score>\0<old>\0<new>\0`       for copy
fn parse_name_status_z(buf: &[u8]) -> Vec<(DiffStatus, PathBuf)> {
    let mut out = Vec::new();
    let mut it = buf.split(|&b| b == 0).filter(|s| !s.is_empty());
    while let Some(status_tok) = it.next() {
        let status_str = String::from_utf8_lossy(status_tok);
        let code = status_str.chars().next().unwrap_or('M');
        match code {
            'R' | 'C' => {
                let (Some(old), Some(new)) = (it.next(), it.next()) else {
                    break;
                };
                let from = path_from_bytes(old);
                let path = path_from_bytes(new);
                let similarity = status_str[1..].parse::<u8>().unwrap_or(0);
                let status = if code == 'R' {
                    DiffStatus::Renamed { from, similarity }
                } else {
                    DiffStatus::Copied { from, similarity }
                };
                out.push((status, path));
            }
            _ => {
                let Some(p) = it.next() else { break };
                let status = match code {
                    'A' => DiffStatus::Added,
                    'D' => DiffStatus::Deleted,
                    // 'M', 'T' (type change), and anything unexpected read
                    // as a plain modify — the safest, least-surprising bucket.
                    _ => DiffStatus::Modified,
                };
                out.push((status, path_from_bytes(p)));
            }
        }
    }
    out
}

/// Raw bytes → `PathBuf` without lossy UTF-8 rewriting (macOS paths are
/// arbitrary bytes). `-z` output is already unquoted, so the bytes are the
/// literal path.
fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b).to_os_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_rename_records() {
        // A added.rs, M mod.rs, D gone.rs, R100 old.rs→new.rs
        let buf = b"A\0added.rs\0M\0mod.rs\0D\0gone.rs\0R100\0old.rs\0new.rs\0";
        let v = parse_name_status_z(buf);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], (DiffStatus::Added, PathBuf::from("added.rs")));
        assert_eq!(v[1], (DiffStatus::Modified, PathBuf::from("mod.rs")));
        assert_eq!(v[2], (DiffStatus::Deleted, PathBuf::from("gone.rs")));
        assert_eq!(
            v[3],
            (
                DiffStatus::Renamed {
                    from: PathBuf::from("old.rs"),
                    similarity: 100
                },
                PathBuf::from("new.rs")
            )
        );
    }

    #[test]
    fn empty_input_yields_no_entries() {
        assert!(parse_name_status_z(b"").is_empty());
    }

    #[test]
    fn copy_record_carries_origin() {
        let buf = b"C75\0src.rs\0copy.rs\0";
        let v = parse_name_status_z(buf);
        assert_eq!(
            v,
            vec![(
                DiffStatus::Copied {
                    from: PathBuf::from("src.rs"),
                    similarity: 75
                },
                PathBuf::from("copy.rs")
            )]
        );
    }
}
