//! Draft a pull-request title + body from the branch's own commits.
//!
//! This is the editable, in-app equivalent of `gh pr create --fill`: the title
//! comes from the branch's first commit subject, the body from the remaining
//! commit subjects as a bullet list. The reviewer can edit both before
//! creating the PR. Best-effort: any git failure degrades to `None` (the dialog
//! then just opens with empty fields).
//!
//! Base resolution mirrors the "Committed on Branch" range: the current
//! branch's `@{upstream}`, else `origin/HEAD` / `origin/main` / `origin/master`.
//! With no base (purely local branch) it falls back to the single HEAD commit.

use std::path::Path;

use crate::process::GitCmd;

/// Resolve the ref the branch's commits are measured against. Mirrors the
/// private `Repository::resolve_branch_base` fallback chain. `None` for a
/// local-only branch with no remote.
async fn resolve_base(workdir: &Path) -> Option<String> {
    if let Ok(raw) = GitCmd::new(workdir)
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
        if let Ok(raw) = GitCmd::new(workdir)
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

/// Read the branch's commit subjects (oldest first) for `<base>..HEAD`, or the
/// single HEAD subject when no base resolves. Empty vec on any failure.
async fn commit_subjects(workdir: &Path) -> Vec<String> {
    let range = resolve_base(workdir)
        .await
        .map(|base| format!("{base}..HEAD"));
    let mut args: Vec<String> = vec!["log".into(), "--reverse".into(), "--format=%s".into()];
    match range {
        Some(r) => args.push(r),
        // No base → just the tip commit, so the dialog still gets a sensible
        // title instead of nothing.
        None => {
            args.push("-1".into());
        }
    }
    let Ok(raw) = GitCmd::new(workdir).args(args).run_raw().await else {
        return Vec::new();
    };
    if !raw.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&raw.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Produce `(title, body)` for the Create-PR dialog from the branch's commits.
/// Title = first commit subject; body = a bullet list of every commit subject
/// (only when there's more than one). `None` when the branch has no commits to
/// summarize (or git failed) — the dialog opens with empty fields.
pub async fn draft_from_commits(workdir: &Path) -> Option<(String, String)> {
    format_draft(&commit_subjects(workdir).await)
}

/// The branch-range diff context an AI drafter reasons over: the same shape as
/// the staged-diff context, but measured across `<base>..HEAD` (the commits the
/// PR will contain) instead of the index. Best-effort — `None` when no base
/// resolves (a purely local branch), when the range is empty, or on any git
/// failure, so the caller can fall back to the deterministic commit draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeContext {
    /// Current branch name, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// `git diff --name-status <base>..HEAD` output (trimmed).
    pub summary: String,
    /// `git diff --patch <base>..HEAD` output.
    pub patch: String,
}

/// Fetch the `<base>..HEAD` diff context for AI PR drafting. `None` when no
/// base resolves, the range has no changes, or git fails — agent drafting
/// requires a resolvable base, and the caller degrades to the commit-subject
/// draft otherwise. Base resolution mirrors [`draft_from_commits`].
pub async fn fetch_range_context(workdir: &Path) -> Option<RangeContext> {
    let base = resolve_base(workdir).await?;
    let range = format!("{base}..HEAD");

    let summary_raw = GitCmd::new(workdir)
        .args(["diff", "--name-status", &range])
        .run_raw()
        .await
        .ok()?;
    if !summary_raw.status.success() {
        return None;
    }
    let summary = String::from_utf8_lossy(&summary_raw.stdout)
        .trim_end()
        .to_string();
    if summary.is_empty() {
        return None;
    }

    let patch_raw = GitCmd::new(workdir)
        .args([
            "diff",
            "--patch",
            "--minimal",
            "--no-color",
            "--no-ext-diff",
            &range,
        ])
        .run_raw()
        .await
        .ok()?;
    if !patch_raw.status.success() {
        return None;
    }
    let patch = String::from_utf8_lossy(&patch_raw.stdout).to_string();

    let branch_raw = GitCmd::new(workdir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .run_raw()
        .await
        .ok();
    let branch = branch_raw.and_then(|raw| {
        if !raw.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&raw.stdout).trim().to_string();
        if name.is_empty() || name == "HEAD" {
            None
        } else {
            Some(name)
        }
    });

    Some(RangeContext {
        branch,
        summary,
        patch,
    })
}

/// Pure title/body shaping from commit subjects (oldest first). Split out for
/// unit testing; see [`draft_from_commits`] for the I/O entry point.
fn format_draft(subjects: &[String]) -> Option<(String, String)> {
    let title = subjects.first()?.clone();
    let body = if subjects.len() > 1 {
        subjects
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    Some((title, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .expect("git setup");
        }
        dir
    }

    fn run_git(workdir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(workdir)
            .status()
            .expect("git run");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn no_commits_yields_none() {
        assert!(format_draft(&[]).is_none());
    }

    #[tokio::test]
    async fn range_context_none_without_base() {
        // A local-only repo with no upstream / origin ref has no resolvable
        // base, so agent drafting is unavailable and the caller falls back.
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "base").expect("write");
        run_git(dir.path(), &["add", "a.txt"]);
        run_git(dir.path(), &["commit", "-m", "base: initial"]);
        assert!(fetch_range_context(dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn range_context_spans_base_to_head() {
        let dir = init_repo();
        std::fs::write(dir.path().join("a.txt"), "base\n").expect("write");
        run_git(dir.path(), &["add", "a.txt"]);
        run_git(dir.path(), &["commit", "-m", "base: initial"]);
        // Stand in for a pushed base branch so resolve_base finds origin/main.
        run_git(dir.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(dir.path().join("b.txt"), "feature line\n").expect("write");
        run_git(dir.path(), &["add", "b.txt"]);
        run_git(dir.path(), &["commit", "-m", "feat: add b"]);

        let ctx = fetch_range_context(dir.path())
            .await
            .expect("range context present");
        assert_eq!(ctx.branch.as_deref(), Some("main"));
        assert!(ctx.summary.contains("b.txt"), "summary names the new file");
        assert!(
            !ctx.summary.contains("a.txt"),
            "base file is outside the range"
        );
        assert!(
            ctx.patch.contains("feature line"),
            "patch carries the range diff"
        );
    }

    #[test]
    fn single_commit_title_only_empty_body() {
        let (title, body) = format_draft(&["feat: add thing".to_string()]).unwrap();
        assert_eq!(title, "feat: add thing");
        assert!(body.is_empty());
    }

    #[test]
    fn multi_commit_bullets_every_subject() {
        let subjects = vec![
            "feat: first".to_string(),
            "fix: second".to_string(),
            "docs: third".to_string(),
        ];
        let (title, body) = format_draft(&subjects).unwrap();
        assert_eq!(title, "feat: first");
        assert_eq!(body, "- feat: first\n- fix: second\n- docs: third");
    }
}
