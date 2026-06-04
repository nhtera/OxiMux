//! Domain types for parsed git diffs.
//!
//! Mirrors the shape `parse_unified_diff` (in `oximux-git`) produces — no
//! parsing logic lives here so any crate can consume `FileDiff` without
//! pulling in tokio or the git CLI wrappers.
//!
//! `large` is a signal, not a hard cap: the parser never truncates; callers
//! (Phase 2 step 9 diff-view UI) decide whether to collapse a long file
//! behind an "expand" affordance.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// File-level classification. Mirrors the headers `git diff` emits before
/// any hunk body, plus `Binary` for the "Binary files X and Y differ" line
/// that suppresses patch output entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed {
        from: PathBuf,
        similarity: u8,
    },
    Copied {
        from: PathBuf,
        similarity: u8,
    },
    /// Mode-only change (e.g. chmod +x). May coexist with hunk content —
    /// `ModeChanged` wins over `Modified` when both headers are present.
    ModeChanged {
        old_mode: u32,
        new_mode: u32,
    },
    Binary,
}

/// One line inside a hunk body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    /// The `\ No newline at end of file` marker; sits in the line stream
    /// next to the line it qualifies so renderers can show the EOF hint
    /// inline with the diff.
    NoNewlineHint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Body text minus the leading `+`/`-`/space marker. UTF-8 only —
    /// binary file bodies never reach this struct (see `DiffStatus::Binary`).
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Free text after the second `@@` (function name, indentation marker,
    /// etc.). Stored verbatim; no further parsing.
    pub header_suffix: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// New path (post-change). For `Deleted`, the original path. For
    /// `Renamed`/`Copied`, the destination — origin lives in `DiffStatus`.
    pub path: PathBuf,
    pub status: DiffStatus,
    pub hunks: Vec<DiffHunk>,
    /// True when the total rendered line count exceeds 1000. The parser
    /// reports this so the UI can collapse oversized diffs; it never
    /// truncates `hunks` itself.
    pub large: bool,
}

/// 1000 visible lines → renderer should collapse by default.
pub const LARGE_DIFF_LINE_THRESHOLD: usize = 1000;

/// Which working-tree partition a file in a combined diff belongs to. Drives
/// staging routing (a region in an `Unstaged` file stages; in a `Staged`
/// file unstages) and the read-at-rest vs stageable distinction (`Committed`
/// is a historical range — read-only). Runs PARALLEL to a combined view's
/// `Vec<FileDiff>` so every file knows its own action side without a second
/// git query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileGroup {
    /// Worktree change not yet staged — Stage / Discard apply.
    Unstaged,
    /// Change already in the index — Unstage applies.
    Staged,
    /// New file git doesn't track yet — whole-file stage only (no per-hunk).
    Untracked,
    /// Already committed (a branch/commit range) — read-only, no staging.
    Committed,
}

/// Request describing which slice of the working tree a combined diff covers.
/// The entry point (a "View all" CTA per SCM section) picks the scope; the
/// git layer assembles the matching ordered `Vec<FileDiff>` + parallel
/// `Vec<FileGroup>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CombinedDiffScope {
    /// Everything in the working tree: unstaged, then staged, then untracked
    /// — one file appearing in both index and worktree shows under BOTH
    /// (each side stages/unstages independently).
    AllChanges,
    /// Unstaged (worktree-vs-index) tracked changes only — the SCM panel's
    /// "Changes" section. Excludes staged and untracked.
    Unstaged,
    /// Staged (index-vs-HEAD) changes only.
    Staged,
    /// Untracked files only.
    Untracked,
    /// A committed revision range (`base..head`), all files — read-only.
    Branch { base: String, head: String },
}

impl CombinedDiffScope {
    /// Human label for the combined-diff tab.
    pub fn title(&self) -> &'static str {
        match self {
            CombinedDiffScope::AllChanges => "All Changes",
            CombinedDiffScope::Unstaged => "Changes",
            CombinedDiffScope::Staged => "Staged Changes",
            CombinedDiffScope::Untracked => "Untracked",
            CombinedDiffScope::Branch { .. } => "Branch Diff",
        }
    }

    /// Stable dedup key for the combined-diff tab. Distinct per scope AND,
    /// for `Branch`, per range — so switching branches opens a fresh tab
    /// instead of silently reactivating a stale one (the `title()` is shared
    /// across all branch ranges, but the range itself isn't). The display
    /// label stays `title()`.
    pub fn tab_key(&self) -> String {
        match self {
            CombinedDiffScope::Branch { base, head } => format!("Branch Diff {base}..{head}"),
            other => other.title().to_string(),
        }
    }
}

/// Result of assembling a combined diff: the merged file list plus a parallel
/// group tag per file (`groups[i]` describes `diffs[i]`). Invariant:
/// `diffs.len() == groups.len()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedDiff {
    pub diffs: Vec<FileDiff>,
    pub groups: Vec<FileGroup>,
}

/// Context lines kept around each change cluster when re-deriving
/// stageable sub-hunks. Matches `git diff -U3` / `git add -p` granularity.
pub const HUNK_CONTEXT: usize = 3;

/// A single stageable change region carved out of a (possibly full-file)
/// `FileDiff`.
///
/// The diff view fetches diffs with full-file context so the user can
/// scroll the whole document — but full context collapses every edit into
/// one giant hunk, which would make "stage this hunk" mean "stage the
/// whole file". `change_regions` re-splits that giant hunk back into the
/// same units `git add -p` would show: each `stage_hunk` is a standalone,
/// `git apply`-ready patch body with ≤`HUNK_CONTEXT` context lines on each
/// side, and `anchor_new` / `anchor_old` locate the region's first changed
/// line so the renderer can dock its action chips next to the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRegion {
    /// Standalone hunk staging only this region. Has correct
    /// `@@ -a,b +c,d @@` counts for the trimmed context window.
    pub stage_hunk: DiffHunk,
    /// New-side line number of the region's first changed line, when that
    /// line is an addition. Renderer anchors the chip bar here.
    pub anchor_new: Option<u32>,
    /// Old-side line number of the region's first changed line, when the
    /// first change is a deletion (no new-side number).
    pub anchor_old: Option<u32>,
}

/// One line of a hunk annotated with the 1-based line numbers it occupies
/// on each side, plus the last number seen on the side it does NOT occupy
/// (`*_before`), which `@@` headers need when a window starts on a change.
struct AnnLine<'a> {
    kind: DiffLineKind,
    content: &'a str,
    old_no: Option<u32>,
    new_no: Option<u32>,
    old_before: u32,
    new_before: u32,
}

/// Re-split a `FileDiff` into the stageable change regions `git add -p`
/// would present. Pure data transform — no I/O, no git.
///
/// All-additions / all-deletions hunks (new files, deleted files, brand
/// new sections) are returned verbatim as one region each — their headers
/// are already correct and there is nothing to re-split. Context-bearing
/// (modify) hunks are split at gaps wider than `2 * HUNK_CONTEXT` context
/// lines, with each cluster trimmed to `HUNK_CONTEXT` surrounding context.
pub fn change_regions(file: &FileDiff) -> Vec<ChangeRegion> {
    let mut regions = Vec::new();
    for hunk in &file.hunks {
        let ann = annotate(hunk);
        let change_idx: Vec<usize> = ann
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed))
            .map(|(i, _)| i)
            .collect();
        if change_idx.is_empty() {
            continue; // pure-context hunk (rename header etc.) — nothing to stage.
        }
        let has_context = ann.iter().any(|l| matches!(l.kind, DiffLineKind::Context));
        if !has_context {
            // All-add or all-del hunk (new file / deleted file / fresh
            // section): the original header is already correct and there is
            // nothing to trim — keep it verbatim as a single region rather
            // than recomputing (recompute would mis-handle `-0,0`).
            let (anchor_new, anchor_old) = change_idx
                .first()
                .map(|&i| (ann[i].new_no, ann[i].old_no))
                .unwrap_or((None, None));
            regions.push(ChangeRegion {
                stage_hunk: hunk.clone(),
                anchor_new,
                anchor_old,
            });
            continue;
        }
        // Cluster the changes: a gap of more than 2*HUNK_CONTEXT context
        // lines starts a new region (same merge rule git uses).
        let mut cluster_start = change_idx[0];
        let mut prev = change_idx[0];
        let flush = |regions: &mut Vec<ChangeRegion>, start: usize, end: usize| {
            let win_start = start.saturating_sub(HUNK_CONTEXT);
            let win_end = (end + HUNK_CONTEXT).min(ann.len() - 1);
            let cidx: Vec<usize> = (win_start..=win_end)
                .filter(|&i| {
                    matches!(ann[i].kind, DiffLineKind::Added | DiffLineKind::Removed)
                })
                .collect();
            regions.push(region_from_window(hunk, &ann, win_start, win_end, &cidx));
        };
        for &i in change_idx.iter().skip(1) {
            if i - prev - 1 > 2 * HUNK_CONTEXT {
                flush(&mut regions, cluster_start, prev);
                cluster_start = i;
            }
            prev = i;
        }
        flush(&mut regions, cluster_start, prev);
    }
    regions
}

/// Annotate a hunk's lines with per-side line numbers. Context advances
/// both sides; Added advances new only; Removed advances old only;
/// NoNewlineHint advances neither.
fn annotate(hunk: &DiffHunk) -> Vec<AnnLine<'_>> {
    let mut old = hunk.old_start.saturating_sub(1);
    let mut new = hunk.new_start.saturating_sub(1);
    let mut out = Vec::with_capacity(hunk.lines.len());
    for l in &hunk.lines {
        let (old_no, new_no, old_before, new_before) = match l.kind {
            DiffLineKind::Context => {
                let (ob, nb) = (old, new);
                old += 1;
                new += 1;
                (Some(old), Some(new), ob, nb)
            }
            DiffLineKind::Added => {
                let (ob, nb) = (old, new);
                new += 1;
                (None, Some(new), ob, nb)
            }
            DiffLineKind::Removed => {
                let (ob, nb) = (old, new);
                old += 1;
                (Some(old), None, ob, nb)
            }
            DiffLineKind::NoNewlineHint => (None, None, old, new),
        };
        out.push(AnnLine {
            kind: l.kind,
            content: &l.content,
            old_no,
            new_no,
            old_before,
            new_before,
        });
    }
    out
}

/// Build a `ChangeRegion` from a window `[start, end]` of annotated lines.
/// `change_idx` are the indices of changed lines inside the window (for
/// the anchor) — may be empty only for the all-context guard, which the
/// caller avoids.
fn region_from_window(
    _hunk: &DiffHunk,
    ann: &[AnnLine<'_>],
    start: usize,
    end: usize,
    change_idx: &[usize],
) -> ChangeRegion {
    let first = &ann[start];
    // A window starting on a change has no number on that change's missing
    // side; fall back to "last seen on that side + 1".
    let old_start = first.old_no.unwrap_or(first.old_before + 1);
    let new_start = first.new_no.unwrap_or(first.new_before + 1);
    let mut old_lines = 0u32;
    let mut new_lines = 0u32;
    let mut lines = Vec::with_capacity(end - start + 1);
    for l in &ann[start..=end] {
        match l.kind {
            DiffLineKind::Context => {
                old_lines += 1;
                new_lines += 1;
            }
            DiffLineKind::Added => new_lines += 1,
            DiffLineKind::Removed => old_lines += 1,
            DiffLineKind::NoNewlineHint => {}
        }
        lines.push(DiffLine {
            kind: l.kind,
            content: l.content.to_string(),
        });
    }
    let stage_hunk = DiffHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        header_suffix: String::new(),
        lines,
    };
    let (anchor_new, anchor_old) = change_idx
        .first()
        .map(|&i| (ann[i].new_no, ann[i].old_no))
        .unwrap_or((None, None));
    ChangeRegion {
        stage_hunk,
        anchor_new,
        anchor_old,
    }
}

#[cfg(test)]
mod region_tests {
    use super::*;

    fn line(kind: DiffLineKind, content: &str) -> DiffLine {
        DiffLine {
            kind,
            content: content.to_string(),
        }
    }

    fn file(hunks: Vec<DiffHunk>) -> FileDiff {
        FileDiff {
            path: "f.txt".into(),
            status: DiffStatus::Modified,
            hunks,
            large: false,
        }
    }

    /// Full-context diff of a 10-line file with line 1 and line 10 edited.
    /// One giant hunk → must split into two regions with git-correct headers.
    #[test]
    fn splits_two_distant_edits() {
        let mut lines = vec![
            line(DiffLineKind::Removed, "line 1"),
            line(DiffLineKind::Added, "LINE 1"),
        ];
        for n in 2..=9 {
            lines.push(line(DiffLineKind::Context, &format!("line {n}")));
        }
        lines.push(line(DiffLineKind::Removed, "line 10"));
        lines.push(line(DiffLineKind::Added, "LINE 10"));
        let f = file(vec![DiffHunk {
            old_start: 1,
            old_lines: 10,
            new_start: 1,
            new_lines: 10,
            header_suffix: String::new(),
            lines,
        }]);

        let regions = change_regions(&f);
        assert_eq!(regions.len(), 2, "8 context lines apart → two regions");

        // Region 0: line 1 edit, 3 trailing context. `@@ -1,4 +1,4 @@`.
        let a = &regions[0].stage_hunk;
        assert_eq!((a.old_start, a.old_lines, a.new_start, a.new_lines), (1, 4, 1, 4));
        assert!(a.lines.iter().any(|l| l.content == "LINE 1"));
        assert!(!a.lines.iter().any(|l| l.content == "LINE 10"));

        // Region 1: line 10 edit, 3 leading context. `@@ -7,4 +7,4 @@`.
        let b = &regions[1].stage_hunk;
        assert_eq!((b.old_start, b.old_lines, b.new_start, b.new_lines), (7, 4, 7, 4));
        assert!(b.lines.iter().any(|l| l.content == "LINE 10"));
        assert!(!b.lines.iter().any(|l| l.content == "LINE 1"));
    }

    /// Two edits within 2*HUNK_CONTEXT context lines stay one region.
    #[test]
    fn merges_near_edits() {
        let f = file(vec![DiffHunk {
            old_start: 1,
            old_lines: 5,
            new_start: 1,
            new_lines: 5,
            header_suffix: String::new(),
            lines: vec![
                line(DiffLineKind::Added, "a"),
                line(DiffLineKind::Context, "c1"),
                line(DiffLineKind::Context, "c2"),
                line(DiffLineKind::Added, "b"),
            ],
        }]);
        assert_eq!(change_regions(&f).len(), 1);
    }

    /// All-additions hunk (new section) is returned verbatim as one region.
    #[test]
    fn all_additions_single_region() {
        let f = file(vec![DiffHunk {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: 2,
            header_suffix: String::new(),
            lines: vec![
                line(DiffLineKind::Added, "x"),
                line(DiffLineKind::Added, "y"),
            ],
        }]);
        let regions = change_regions(&f);
        assert_eq!(regions.len(), 1);
        let h = &regions[0].stage_hunk;
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (0, 0, 1, 2));
    }
}
