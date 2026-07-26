//! Range computation for Shift+Click multi-select in the SCM panel.
//!
//! Pure helper — given the flat visible-row order and two anchor paths,
//! returns the inclusive path range between them in flat order. Lives
//! outside the GPUI render path so the routing logic stays testable in
//! isolation.
//!
//! "Flat order" is what `changed_files::render_sections` walks top-to-
//! bottom: CHANGES (unstaged) → STAGED CHANGES → UNTRACKED FILES,
//! preserving each section's own ordering. The caller flattens those
//! sections into a `Vec<PathBuf>` before invoking [`range_between`].
//!
//! Partial-stage rows (one path mirrored into both Unstaged and Staged)
//! may appear twice in the flat list. [`range_between`] tolerates that
//! by deduping the returned vec — selection is keyed by path, not by
//! row instance, so a partial-stage path is "one selection" regardless
//! of which mirrored row the user clicked.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Inclusive range between `anchor` and `target` in `rows`'s flat order.
///
/// Behaviour:
/// - Both ends must be present. If either is missing from `rows`, returns
///   just the present end (or empty if both are missing). Matches
///   "Shift+Click on a stale row" — the user gets the click target only,
///   the renderer's next pass will refresh `rows`.
/// - Order-agnostic: anchor before or after target yields the same set.
///   The returned `Vec` is always in flat (row-walk) order so callers
///   can splice it into a `HashSet` without caring which end was clicked
///   first.
/// - Deduplicates on output. A path that appears twice in `rows`
///   (partial-stage) lands in the result once.
/// - `anchor == target`: returns just that path (or empty if absent).
/// - Empty `rows`: returns empty.
pub fn range_between(rows: &[PathBuf], anchor: &Path, target: &Path) -> Vec<PathBuf> {
    if rows.is_empty() {
        return Vec::new();
    }

    let first_pos = |needle: &Path| rows.iter().position(|p| p.as_path() == needle);
    let (anchor_pos, target_pos) = match (first_pos(anchor), first_pos(target)) {
        (Some(a), Some(t)) => (a, t),
        // One end missing: hand back the present end only. Caller gets
        // graceful "shift-click on a stale row picks the click target".
        (Some(a), None) => return dedupe(vec![rows[a].clone()]),
        (None, Some(t)) => return dedupe(vec![rows[t].clone()]),
        (None, None) => return Vec::new(),
    };

    let (lo, hi) = if anchor_pos <= target_pos {
        (anchor_pos, target_pos)
    } else {
        (target_pos, anchor_pos)
    };
    dedupe(rows[lo..=hi].to_vec())
}

/// Preserve flat-order positions while collapsing duplicate paths to
/// their first occurrence. `HashSet::insert`'s bool return flags the
/// first sighting; subsequent dups are dropped.
fn dedupe(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(paths.len());
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(s: &[&str]) -> Vec<PathBuf> {
        s.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn forward_range_returns_inclusive_slice() {
        let rows = paths(&["a", "b", "c", "d"]);
        let got = range_between(&rows, Path::new("a"), Path::new("c"));
        assert_eq!(got, paths(&["a", "b", "c"]));
    }

    #[test]
    fn reverse_range_returns_same_set_in_flat_order() {
        // User clicks "c" first, then Shift+Clicks "a" — selection is
        // still a..c in flat order, not c..a.
        let rows = paths(&["a", "b", "c", "d"]);
        let got = range_between(&rows, Path::new("c"), Path::new("a"));
        assert_eq!(got, paths(&["a", "b", "c"]));
    }

    #[test]
    fn full_range_first_to_last() {
        let rows = paths(&["a", "b", "c", "d"]);
        let got = range_between(&rows, Path::new("a"), Path::new("d"));
        assert_eq!(got, paths(&["a", "b", "c", "d"]));
    }

    #[test]
    fn single_path_range_is_one_path() {
        let rows = paths(&["a", "b", "c"]);
        let got = range_between(&rows, Path::new("b"), Path::new("b"));
        assert_eq!(got, paths(&["b"]));
    }

    #[test]
    fn empty_rows_returns_empty() {
        let rows: Vec<PathBuf> = Vec::new();
        let got = range_between(&rows, Path::new("a"), Path::new("b"));
        assert!(got.is_empty());
    }

    #[test]
    fn anchor_missing_falls_back_to_target() {
        let rows = paths(&["a", "b", "c"]);
        let got = range_between(&rows, Path::new("ghost"), Path::new("b"));
        assert_eq!(got, paths(&["b"]));
    }

    #[test]
    fn target_missing_falls_back_to_anchor() {
        let rows = paths(&["a", "b", "c"]);
        let got = range_between(&rows, Path::new("a"), Path::new("ghost"));
        assert_eq!(got, paths(&["a"]));
    }

    #[test]
    fn both_missing_returns_empty() {
        let rows = paths(&["a", "b", "c"]);
        let got = range_between(&rows, Path::new("ghost1"), Path::new("ghost2"));
        assert!(got.is_empty());
    }

    #[test]
    fn dedupes_partial_stage_mirror() {
        // Partial-stage path appears in both Unstaged and Staged
        // sections; the flat list carries it twice. Range output
        // collapses to one entry.
        let rows = paths(&["a", "b", "b", "c"]);
        let got = range_between(&rows, Path::new("a"), Path::new("c"));
        assert_eq!(got, paths(&["a", "b", "c"]));
    }

    #[test]
    fn range_through_partial_stage_mirror() {
        // Anchor on the unstaged copy, target on the staged copy —
        // selection still collapses to one path.
        let rows = paths(&["a", "shared", "x", "shared", "z"]);
        let got = range_between(&rows, Path::new("shared"), Path::new("shared"));
        // First sighting wins; the inclusive range is rows[1..=1] →
        // ["shared"]. (If the renderer ever needs to start from the
        // *staged* mirror specifically, it can pass the staged-side
        // index instead of the path; the helper stays pure on paths.)
        assert_eq!(got, paths(&["shared"]));
    }

    #[test]
    fn range_spanning_section_boundary_dedupes_internal_mirror() {
        // Walk across the (unstaged → staged) boundary with one
        // mirrored row between the endpoints. Output collapses to one
        // entry per path while preserving the visit order.
        let rows = paths(&["u1", "mirror", "s1", "mirror", "s2"]);
        let got = range_between(&rows, Path::new("u1"), Path::new("s2"));
        assert_eq!(got, paths(&["u1", "mirror", "s1", "s2"]));
    }
}
