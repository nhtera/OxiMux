//! Per-file read decision. Transcripts are append-only in practice, so a file
//! that only grew is read from where the last pass stopped; anything else
//! (truncated, rewritten, replaced) is re-read from the top.

/// What the index remembers about one transcript file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watermark {
    /// Bytes consumed — always the end of a complete line.
    pub offset: u64,
    pub mtime_ms: i64,
    pub size: u64,
    pub session_row_id: Option<i64>,
    pub fail_count: i64,
    pub failed_mtime_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub mtime_ms: i64,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    /// Unchanged since the last pass.
    Skip,
    /// Failed repeatedly at this exact mtime; wait for the file to change.
    HeldOut,
    /// Grew: read only the bytes after `offset`.
    Append { offset: u64 },
    /// New, shrunk, or rewritten: drop what is indexed and read it all.
    Replace,
}

/// Failures at one mtime before a file is held out.
pub const MAX_FAILURES: i64 = 3;

/// Decide how to read a file. `newline_before` reports whether the byte just
/// before an offset is `\n` — the cheap check that the indexed head is still
/// the head we indexed.
pub fn decide(prev: Option<&Watermark>, stat: Stat, newline_before: impl FnOnce(u64) -> bool) -> ReadPlan {
    let Some(prev) = prev else { return ReadPlan::Replace };
    if prev.fail_count >= MAX_FAILURES && prev.failed_mtime_ms == Some(stat.mtime_ms) {
        return ReadPlan::HeldOut;
    }
    // A failure at an mtime the file no longer has is retried even when the
    // size matches.
    let failed = prev.fail_count > 0;
    if !failed && stat.size == prev.size && stat.mtime_ms == prev.mtime_ms {
        return ReadPlan::Skip;
    }
    if prev.offset > 0 && stat.size >= prev.offset && newline_before(prev.offset) {
        return ReadPlan::Append { offset: prev.offset };
    }
    ReadPlan::Replace
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm(offset: u64, size: u64, mtime: i64) -> Watermark {
        Watermark { offset, mtime_ms: mtime, size, session_row_id: Some(1), fail_count: 0, failed_mtime_ms: None }
    }

    #[test]
    fn new_file_is_read_whole() {
        assert_eq!(decide(None, Stat { mtime_ms: 1, size: 10 }, |_| true), ReadPlan::Replace);
    }

    #[test]
    fn unchanged_file_is_skipped() {
        assert_eq!(decide(Some(&wm(10, 10, 5)), Stat { mtime_ms: 5, size: 10 }, |_| panic!()), ReadPlan::Skip);
    }

    #[test]
    fn grown_file_appends_from_the_offset() {
        let plan = decide(Some(&wm(10, 10, 5)), Stat { mtime_ms: 6, size: 30 }, |o| o == 10);
        assert_eq!(plan, ReadPlan::Append { offset: 10 });
    }

    #[test]
    fn shrunk_or_rewritten_file_is_replaced() {
        assert_eq!(decide(Some(&wm(10, 10, 5)), Stat { mtime_ms: 6, size: 4 }, |_| true), ReadPlan::Replace);
        assert_eq!(decide(Some(&wm(10, 10, 5)), Stat { mtime_ms: 6, size: 30 }, |_| false), ReadPlan::Replace);
    }

    #[test]
    fn repeated_failures_hold_out_until_the_file_changes() {
        let mut w = wm(0, 10, 5);
        w.fail_count = MAX_FAILURES;
        w.failed_mtime_ms = Some(5);
        assert_eq!(decide(Some(&w), Stat { mtime_ms: 5, size: 10 }, |_| true), ReadPlan::HeldOut);
        assert_eq!(decide(Some(&w), Stat { mtime_ms: 7, size: 10 }, |_| true), ReadPlan::Replace);
        // Fewer failures: retried even with an unchanged stat.
        w.fail_count = 1;
        assert_eq!(decide(Some(&w), Stat { mtime_ms: 5, size: 10 }, |_| true), ReadPlan::Replace);
    }
}
