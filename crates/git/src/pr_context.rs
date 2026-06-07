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

    #[test]
    fn no_commits_yields_none() {
        assert!(format_draft(&[]).is_none());
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
