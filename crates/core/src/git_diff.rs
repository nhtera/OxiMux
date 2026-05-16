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
