//! Compact CI-checks summary row for the Source Control panel.
//!
//! Renders a single line — "CI passing/running/failing  ✓N ✗N ●N" — derived
//! from the PR's `gh pr checks` runs. The check runs are fetched (and the row
//! refreshed) by the panel's state observer on the same ~30s throttle as the
//! PR status, so there is no dedicated poll task here: this module is pure
//! summary + render, the panel owns the data + cadence.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use crate::shell::forge::CheckRun;
use oximux_settings::{Theme, Typography};

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

/// Render the compact CI row, or `None` when there are no checks to show (no
/// PR, or a PR with zero check runs) so the panel doesn't reserve dead space.
pub fn render_ci_row(
    summary: CheckSummary,
    theme: Theme,
    typography: &Typography,
) -> Option<AnyElement> {
    if !summary.is_renderable() {
        return None;
    }
    let headline_color = if summary.fail > 0 {
        theme.status_error
    } else if summary.pending > 0 {
        theme.status_warning
    } else {
        theme.status_ok
    };

    // Build the "✓N ✗N ●N" tallies, omitting any zero bucket.
    let mut counts = div().flex().flex_row().items_center().gap(px(8.0));
    if summary.pass > 0 {
        counts = counts.child(
            div()
                .text_color(theme.status_ok)
                .child(format!("✓{}", summary.pass)),
        );
    }
    if summary.fail > 0 {
        counts = counts.child(
            div()
                .text_color(theme.status_error)
                .child(format!("✗{}", summary.fail)),
        );
    }
    if summary.pending > 0 {
        counts = counts.child(
            div()
                .text_color(theme.status_warning)
                .child(format!("●{}", summary.pending)),
        );
    }

    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(typography.t_sub_label))
            .child(div().text_color(headline_color).child(summary.headline()))
            .child(counts)
            .into_any_element(),
    )
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
