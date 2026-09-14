//! Ahead/behind of HEAD against the ref a worktree is measured from.
//!
//! The rail wants one number pair per worktree — `↑2 ↓5` — and, just as
//! importantly, the *name* of what it was compared to, because a count
//! against the wrong base is worse than no count. So the answer here is a
//! [`AheadBehind`] carrying its base, and "no base resolves" is `None`, never
//! a zero pair: an unpublished branch in a remote-less repo has nothing to be
//! behind, and the row must be able to show nothing rather than `↑0 ↓0`.
//!
//! Resolution order, first hit wins:
//!
//! 1. the caller's pinned base — the SCM panel's per-worktree base ref;
//! 2. the checked-out branch's configured upstream, which is what the git
//!    panel's own ahead/behind is relative to, so the two agree;
//! 3. the project's default branch, as a local ref and then as
//!    `origin/<default>` — the case an upstream cannot answer: a branch
//!    that was never pushed, or whose upstream is gone after a merge.
//!
//! Cost, per call: one `for-each-ref` over the local branches (it yields
//! HEAD's branch, its upstream's name and its ahead/behind in one process),
//! then one `rev-list --left-right --count` per remaining candidate until one
//! resolves. Nothing here walks the working tree, and a repo with an
//! upstream pays exactly one process.

use std::path::Path;
use std::time::Duration;

use crate::error::Result;
use crate::process::GitCmd;

/// Commit counts of HEAD relative to a named base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AheadBehind {
    /// The ref the counts are against, as the user would name it
    /// (`origin/main`, `main`, a pinned `release/2.1`).
    pub base: String,
    /// Commits on HEAD that are not on `base`.
    pub ahead: u32,
    /// Commits on `base` that are not on HEAD.
    pub behind: u32,
}

/// `rev-list` is a pure ref walk, but a pathological history can still take
/// a while; the rail polls, so a slow answer must not pile up.
const REV_LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// HEAD's ahead/behind against the first base that resolves, in the order
/// the module docs give.
///
/// Two kinds of "no answer", kept apart on purpose: `Ok(None)` means every
/// candidate was consulted and none names a commit — a real property of the
/// repo, safe to cache. `Err` means git itself could not be run or timed out
/// at some step, which says nothing about the repo; a caller should keep
/// whatever it last knew rather than record "no base" or, worse, skip the
/// upstream and report against the default branch.
pub async fn ahead_behind_vs_base(
    workdir: &Path,
    pinned_base: Option<&str>,
    default_branch: &str,
) -> Result<Option<AheadBehind>> {
    if let Some(pinned) = pinned_base.map(str::trim).filter(|s| !s.is_empty())
        && let Some(found) = ahead_behind_against(workdir, pinned).await?
    {
        return Ok(Some(found));
    }
    if let Some(found) = ahead_behind_vs_upstream(workdir).await? {
        return Ok(Some(found));
    }
    let default_branch = default_branch.trim();
    if default_branch.is_empty() {
        return Ok(None);
    }
    for cand in [default_branch.to_string(), format!("origin/{default_branch}")] {
        if let Some(found) = ahead_behind_against(workdir, &cand).await? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `git rev-list --left-right --count <base>...HEAD`. `Ok(None)` when `base`
/// does not name a commit (unknown ref, no HEAD yet, not a repo); `Err` when
/// git could not be run or timed out.
pub async fn ahead_behind_against(workdir: &Path, base: &str) -> Result<Option<AheadBehind>> {
    let raw = GitCmd::new(workdir)
        .args(["rev-list", "--left-right", "--count", &format!("{base}...HEAD"), "--"])
        .timeout(REV_LIST_TIMEOUT)
        .run_raw()
        .await?;
    if !raw.status.success() {
        return Ok(None);
    }
    Ok(
        parse_left_right_count(&String::from_utf8_lossy(&raw.stdout)).map(|(behind, ahead)| {
            AheadBehind {
                base: base.to_string(),
                ahead,
                behind,
            }
        }),
    )
}

/// HEAD's ahead/behind against its branch's configured upstream, from one
/// `for-each-ref` over the local branches: the line flagged `*` is the
/// branch this worktree has checked out (a linked worktree's own HEAD, since
/// git resolves it per worktree), and its upstream's short name and track
/// summary (`ahead 2, behind 1`, empty when level) come in the same process.
/// `Ok(None)` when HEAD is detached, the branch has no upstream, or its
/// upstream is gone (deleted after a merge). All three mean "compare to
/// something else"; only a git that could not run is an `Err`.
///
/// Enumerating `refs/heads/` rather than naming one ref is deliberate: it is
/// HEAD-relative, so a checkout the app did not make (a `git switch` in that
/// worktree's terminal) is measured as it is, not as the row remembers it —
/// and a one-ref query would match by prefix (`refs/heads/feat` also lists
/// `feat/x`).
async fn ahead_behind_vs_upstream(workdir: &Path) -> Result<Option<AheadBehind>> {
    let raw = GitCmd::new(workdir)
        .args([
            "for-each-ref",
            "--format=%(HEAD)%09%(upstream:short)%09%(upstream:track,nobracket)",
            "refs/heads/",
        ])
        .run_raw()
        .await?;
    if !raw.status.success() {
        return Ok(None);
    }
    Ok(parse_head_upstream_track(&String::from_utf8_lossy(&raw.stdout)))
}

/// Find the `*`-flagged line of the `for-each-ref` format above and parse
/// its `<upstream>\t<track>`. `track` is `ahead N`, `behind M`,
/// `ahead N, behind M`, empty (level) or `gone`. No upstream prints an empty
/// name; a detached HEAD flags no line at all.
fn parse_head_upstream_track(text: &str) -> Option<AheadBehind> {
    let line = text.lines().find_map(|l| l.strip_prefix("*\t"))?;
    let (name, track) = line.split_once('\t')?;
    let name = name.trim();
    if name.is_empty() || track.trim() == "gone" {
        return None;
    }
    let (mut ahead, mut behind) = (0u32, 0u32);
    for part in track.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => ahead = n,
            (Some("behind"), Some(n)) => behind = n,
            (None, _) => {}
            _ => return None,
        }
    }
    Some(AheadBehind {
        base: name.to_string(),
        ahead,
        behind,
    })
}

/// Parse `<left>\t<right>` from `rev-list --left-right --count`. Left is the
/// base's side (commits HEAD is behind), right is HEAD's (commits ahead).
fn parse_left_right_count(text: &str) -> Option<(u32, u32)> {
    let mut it = text.split_whitespace();
    let left = it.next()?.parse().ok()?;
    let right = it.next()?.parse().ok()?;
    Some((left, right))
}

#[cfg(test)]
mod tests {
    use super::{parse_head_upstream_track, parse_left_right_count};

    #[test]
    fn parses_the_tab_separated_pair() {
        assert_eq!(parse_left_right_count("5\t2\n"), Some((5, 2)));
        assert_eq!(parse_left_right_count("0\t0\n"), Some((0, 0)));
    }

    #[test]
    fn upstream_track_covers_every_shape_git_prints() {
        let ab = |t: &str| parse_head_upstream_track(t).map(|a| (a.base, a.ahead, a.behind));
        assert_eq!(ab("*\torigin/x\tahead 2, behind 1\n"), Some(("origin/x".into(), 2, 1)));
        assert_eq!(ab("*\torigin/x\tahead 3\n"), Some(("origin/x".into(), 3, 0)));
        assert_eq!(ab("*\torigin/x\tbehind 4\n"), Some(("origin/x".into(), 0, 4)));
        assert_eq!(ab("*\torigin/x\t\n"), Some(("origin/x".into(), 0, 0)));
        assert_eq!(ab("*\torigin/x\tgone\n"), None, "a deleted upstream is not a base");
        assert_eq!(ab("*\t\t\n"), None, "no upstream configured");
        assert_eq!(ab(" \torigin/y\tahead 9\n"), None, "detached HEAD flags no branch");
        assert_eq!(ab(""), None, "no branches at all");
    }

    #[test]
    fn only_the_checked_out_branch_is_read() {
        let text = " \torigin/other\tahead 7\n*\torigin/mine\tbehind 2\n \t\t\n";
        let ab = parse_head_upstream_track(text).unwrap();
        assert_eq!((ab.base.as_str(), ab.ahead, ab.behind), ("origin/mine", 0, 2));
    }

    #[test]
    fn rejects_anything_that_is_not_two_counts() {
        assert_eq!(parse_left_right_count(""), None);
        assert_eq!(parse_left_right_count("7\n"), None);
        assert_eq!(parse_left_right_count("a\tb\n"), None);
    }
}
