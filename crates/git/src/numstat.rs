//! Parser + runner for `git diff --numstat -z HEAD`.
//!
//! Numstat surfaces per-file `(added, removed)` line counts vs HEAD for the
//! whole working tree (staged + unstaged combined). Phase 02 piggybacks this
//! onto the existing status poll so each `FileStatus` can carry `+A −B`
//! line counts without spawning per-row diff tasks.
//!
//! With `-z`, two record shapes appear:
//!
//! - Plain file:  `<added>\t<removed>\t<path>\0`
//! - Rename:      `<added>\t<removed>\t\0<orig_path>\0<new_path>\0`
//!
//! Binary files emit `-` in place of the numbers; we map both columns to
//! `None`-equivalent (the parser drops binaries from the result map since
//! the UI has no use for non-numeric counts).
//!
//! Records are robust to:
//! - Trailing data without a final NUL (parser yields the residue).
//! - Renames where the two-NUL form is interspersed with plain records.
//! - Empty `git diff --numstat` output (no HEAD yet) → empty map.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 30 s ceiling on the numstat invocation. Numstat doesn't compute hunks,
/// just runs the diff algorithm with `--no-renames`-style output, so it's
/// cheaper than `diff -p` — but a pathological repo can still take seconds.
const NUMSTAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `git diff --numstat -z HEAD` against `workdir` and parse the output
/// into a `path → (added, removed)` map.
///
/// Returns an empty map when there's no HEAD yet (initial repo) or when
/// HEAD lookup fails for any other reason. Numstat is a best-effort
/// enrichment for status rows; a failure here must NOT poison the
/// surrounding status poll.
pub async fn diff_numstat_head(workdir: &Path) -> Result<HashMap<PathBuf, (u32, u32)>> {
    let raw = GitCmd::new(workdir)
        .timeout(NUMSTAT_TIMEOUT)
        .args(["diff", "--numstat", "-z", "HEAD"])
        .run_raw()
        .await?;
    if !raw.status.success() {
        // No HEAD / pre-first-commit / detached weirdness — return empty
        // rather than propagate. The status panel still renders, just
        // without the `+A −B` decoration on rows.
        return Ok(HashMap::new());
    }
    parse_numstat_z(&raw.stdout)
}

/// Run `git diff --numstat -z -M <base> <head>` and parse into a
/// `path → (added, removed)` map. Used by the "Committed on Branch"
/// section to decorate each branch-committed file with its net line
/// counts. Best-effort: a non-zero exit (bad range, no HEAD) yields an
/// empty map rather than propagating.
pub async fn diff_numstat_range(
    workdir: &Path,
    base: &str,
    head: &str,
) -> Result<HashMap<PathBuf, (u32, u32)>> {
    let raw = GitCmd::new(workdir)
        .timeout(NUMSTAT_TIMEOUT)
        .args(["diff", "--numstat", "-z", "-M", base, head])
        .run_raw()
        .await?;
    if !raw.status.success() {
        return Ok(HashMap::new());
    }
    parse_numstat_z(&raw.stdout)
}

/// Run `git diff --numstat -z <sha>^..<sha>` and aggregate the per-file
/// `(added, removed)` counts into a single `(total_added, total_removed)`
/// pair for the commit. Feeds the commit-graph row's hover tooltip with
/// a one-line size signal so the user can scan the panel and tell at a
/// glance which commits were three-line fixes vs thousand-line drops.
///
/// Returns `Ok((0, 0))` when the commit is a root (no parent, so the
/// `sha^..sha` range fails), the file was renamed-only with no text
/// delta, or git fails for any other reason — graceful degradation so
/// the tooltip silently omits the stats line instead of surfacing a
/// "git failed" toast for a non-functional decoration.
pub async fn diff_numstat_commit(workdir: &Path, sha: &str) -> Result<(u32, u32)> {
    let range = format!("{sha}^..{sha}");
    let raw = GitCmd::new(workdir)
        .timeout(NUMSTAT_TIMEOUT)
        .args(["diff", "--numstat", "-z", &range])
        .run_raw()
        .await?;
    if !raw.status.success() {
        // Root commit or a transient git failure — return zeros rather
        // than propagate. The tooltip caller treats `(0, 0)` the same
        // as a successful no-change diff, which is the right
        // user-visible behavior for a root commit (no parent to diff
        // against = no stats to show).
        return Ok((0, 0));
    }
    let map = parse_numstat_z(&raw.stdout)?;
    let totals = map
        .values()
        .fold((0u32, 0u32), |(a, r), &(na, nr)| (a + na, r + nr));
    Ok(totals)
}

/// Pure parser. Public so the unit tests can drive it with literal byte
/// fixtures (and so future call sites — e.g. per-file numstat on hover —
/// can reuse the logic).
pub fn parse_numstat_z(buf: &[u8]) -> Result<HashMap<PathBuf, (u32, u32)>> {
    let mut out: HashMap<PathBuf, (u32, u32)> = HashMap::new();
    let mut iter = RecordIter::new(buf);

    while let Some(rec) = iter.next_record() {
        if rec.is_empty() {
            continue;
        }
        // A record is one of:
        //   "added\tremoved\tpath"   → plain file, path is the last column
        //   "added\tremoved\t"       → rename leader; next two records are
        //                              orig_path then new_path
        let s =
            std::str::from_utf8(rec).map_err(|e| GitError::parse(format!("numstat utf-8: {e}")))?;
        let (added_s, rest) = s
            .split_once('\t')
            .ok_or_else(|| GitError::parse(format!("numstat missing \\t1: {s:?}")))?;
        let (removed_s, path_part) = rest
            .split_once('\t')
            .ok_or_else(|| GitError::parse(format!("numstat missing \\t2: {rest:?}")))?;

        // Binary diffs emit "-\t-\tpath" — skip them; the UI has nothing
        // numeric to render for binaries.
        if added_s == "-" || removed_s == "-" {
            // Consume rename trailer if this was the (rare) binary rename
            // form so we don't desync the iterator.
            if path_part.is_empty() {
                // skip orig + new
                let _ = iter.next_record();
                let _ = iter.next_record();
            }
            continue;
        }
        let added: u32 = added_s
            .parse()
            .map_err(|e| GitError::parse(format!("numstat added: {e}")))?;
        let removed: u32 = removed_s
            .parse()
            .map_err(|e| GitError::parse(format!("numstat removed: {e}")))?;

        if path_part.is_empty() {
            // Rename: the next two NUL chunks are orig then new. We index
            // by the NEW path because porcelain v2's `FileStatus::path` is
            // also the new path — same key, so merge lines up.
            let _orig = iter
                .next_record()
                .ok_or_else(|| GitError::parse("numstat rename missing orig"))?;
            let new = iter
                .next_record()
                .ok_or_else(|| GitError::parse("numstat rename missing new"))?;
            let new_str = std::str::from_utf8(new)
                .map_err(|e| GitError::parse(format!("numstat new path utf-8: {e}")))?;
            out.insert(PathBuf::from(new_str), (added, removed));
        } else {
            out.insert(PathBuf::from(path_part), (added, removed));
        }
    }
    Ok(out)
}

/// NUL-delimited record iterator. Identical shape to the one in `status.rs`
/// but local-private to keep both modules independent — copying ten lines
/// is cheaper than carving out a shared `record_iter` module that two
/// callers depend on.
struct RecordIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> RecordIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn next_record(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let rest = &self.buf[self.pos..];
        match rest.iter().position(|&b| b == 0) {
            Some(i) => {
                let r = &rest[..i];
                self.pos += i + 1;
                Some(r)
            }
            None => {
                let r = rest;
                self.pos = self.buf.len();
                Some(r)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty_map() {
        let m = parse_numstat_z(b"").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn single_plain_file() {
        let m = parse_numstat_z(b"3\t4\tsrc/main.rs\x00").unwrap();
        assert_eq!(m.get(&PathBuf::from("src/main.rs")), Some(&(3, 4)));
    }

    #[test]
    fn multiple_plain_files() {
        let buf = b"5\t0\tnew.rs\x00\
            0\t12\tdeleted.txt\x00\
            7\t3\tnested/dir/file.md\x00";
        let m = parse_numstat_z(buf).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[&PathBuf::from("new.rs")], (5, 0));
        assert_eq!(m[&PathBuf::from("deleted.txt")], (0, 12));
        assert_eq!(m[&PathBuf::from("nested/dir/file.md")], (7, 3));
    }

    #[test]
    fn rename_uses_new_path_as_key() {
        // -z rename form: "added\tremoved\t\0orig\0new\0".
        let buf = b"2\t1\t\x00old/name.rs\x00new/name.rs\x00";
        let m = parse_numstat_z(buf).unwrap();
        assert_eq!(m.len(), 1, "rename collapses to one entry");
        assert_eq!(m.get(&PathBuf::from("new/name.rs")), Some(&(2, 1)));
        assert!(
            !m.contains_key(&PathBuf::from("old/name.rs")),
            "orig path must NOT be indexed"
        );
    }

    #[test]
    fn binary_files_are_dropped() {
        // Binary entries emit `-\t-\tpath` — they would deserialise as junk;
        // the UI has nothing to do with them.
        let buf = b"-\t-\tassets/logo.png\x00\
            3\t1\tsrc/lib.rs\x00";
        let m = parse_numstat_z(buf).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m.contains_key(&PathBuf::from("src/lib.rs")));
        assert!(!m.contains_key(&PathBuf::from("assets/logo.png")));
    }

    #[test]
    fn binary_rename_consumes_trailer_without_desync() {
        // A binary rename's leader is "-\t-\t\0" followed by orig\0new\0
        // — we must consume the trailer so a following plain record
        // parses correctly.
        let buf = b"-\t-\t\x00old.png\x00new.png\x00\
            8\t2\tsrc/lib.rs\x00";
        let m = parse_numstat_z(buf).unwrap();
        assert_eq!(m.len(), 1, "binary rename dropped, plain file kept");
        assert_eq!(m.get(&PathBuf::from("src/lib.rs")), Some(&(8, 2)));
    }

    #[test]
    fn path_with_tabs_in_name_is_misread_but_does_not_panic() {
        // Numstat doesn't escape tabs in paths under `-z`, but a path
        // containing a literal tab is exceedingly rare on macOS file
        // systems. Document the limit: we'd parse the tab as a column
        // separator and either error out or attribute the count to the
        // wrong path. Either way: must NOT panic. This is the safety net.
        let buf = b"3\t4\tweird\tpath.rs\x00";
        // Parser sees three tabs total: it pulls "3","4" then "weird"
        // as the path; the trailing "path.rs" becomes a malformed record.
        let res = parse_numstat_z(buf);
        // Either parses with path="weird" (graceful) or errors (also
        // graceful) — both are acceptable; what's NOT acceptable is panic.
        // Just ensure we got a Result back without dropping the harness.
        let _ = res;
    }
}
