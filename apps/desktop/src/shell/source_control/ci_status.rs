//! CI-checks tallies for the Source Control panel.
//!
//! `CheckSummary` reduces a PR's `gh pr checks` runs into pass / fail / pending
//! / other counts and a worst-first headline. The check runs are fetched by the
//! panel's state observer on the same ~30s throttle as the PR status; this
//! module is pure summary, the panel owns the data + cadence and the
//! `checks_section` module renders it.

use crate::shell::forge::CheckRun;

/// Pass / fail / pending tallies derived from a PR's check runs. `other`
/// absorbs gh's `skipping` and `cancel` buckets (and anything unrecognized)
/// so they don't masquerade as passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckSummary {
    pub pass: usize,
    pub fail: usize,
    pub pending: usize,
    pub other: usize,
}

impl CheckSummary {
    pub fn from_runs(runs: &[CheckRun]) -> Self {
        let mut s = Self::default();
        for r in runs {
            match r.bucket.as_str() {
                "pass" => s.pass += 1,
                "fail" => s.fail += 1,
                "pending" => s.pending += 1,
                _ => s.other += 1,
            }
        }
        s
    }

    pub fn total(&self) -> usize {
        self.pass + self.fail + self.pending + self.other
    }

    /// Whether the compact row should render. False when there are no checks
    /// at all, or only skipped/cancelled ones — a green "CI passing" with no
    /// badges would be misleading, so the row collapses instead.
    pub fn is_renderable(&self) -> bool {
        self.pass > 0 || self.fail > 0 || self.pending > 0
    }

    /// One-word overall status, worst-first: any failure dominates, then any
    /// pending, else passing.
    pub fn headline(&self) -> &'static str {
        if self.fail > 0 {
            "CI failing"
        } else if self.pending > 0 {
            "CI running"
        } else {
            "CI passing"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bucket: &str) -> CheckRun {
        CheckRun {
            name: "c".into(),
            bucket: bucket.into(),
            link: String::new(),
            description: String::new(),
        }
    }

    #[test]
    fn summary_tallies_by_bucket() {
        let runs = vec![run("pass"), run("pass"), run("fail"), run("pending"), run("skipping")];
        let s = CheckSummary::from_runs(&runs);
        assert_eq!((s.pass, s.fail, s.pending, s.other), (2, 1, 1, 1));
        assert_eq!(s.total(), 5);
    }

    #[test]
    fn headline_is_worst_first() {
        assert_eq!(
            CheckSummary { pass: 1, fail: 1, pending: 1, other: 0 }.headline(),
            "CI failing"
        );
        assert_eq!(
            CheckSummary { pass: 1, fail: 0, pending: 1, other: 0 }.headline(),
            "CI running"
        );
        assert_eq!(
            CheckSummary { pass: 3, fail: 0, pending: 0, other: 0 }.headline(),
            "CI passing"
        );
    }

    #[test]
    fn empty_summary_is_not_renderable() {
        assert!(!CheckSummary::default().is_renderable());
    }

    #[test]
    fn all_skipped_is_not_renderable() {
        // Only `other` (skipped/cancelled) → row collapses despite total() > 0.
        let s = CheckSummary::from_runs(&[run("skipping"), run("cancel")]);
        assert_eq!(s.total(), 2);
        assert!(!s.is_renderable());
    }

    #[test]
    fn any_real_check_is_renderable() {
        assert!(CheckSummary::from_runs(&[run("pending")]).is_renderable());
        assert!(CheckSummary::from_runs(&[run("pass")]).is_renderable());
        assert!(CheckSummary::from_runs(&[run("fail")]).is_renderable());
    }
}
