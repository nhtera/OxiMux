//! Unified diff parser for `git diff -p` output.
//!
//! Pure sync function on `&str` → `Vec<FileDiff>`. Called from async
//! `Repository::diff_*` methods after the `git` process has run. No tokio,
//! no I/O, no GPUI — domain types live in `oximux_core::git_diff`.
//!
//! SECURITY: v1 only consumes diff text produced locally by `git diff` on a
//! validated `Repository`, so this parser is not a trust boundary. If a
//! future feature ("apply patch from clipboard / network", `git am` flow)
//! lands, paths inside `parse_diff_git_paths` / `+++ b/...` lines become
//! attacker-controlled — at that point sanitise against directory escapes
//! (`..`, absolute paths, symlink traversal) before constructing any
//! `PathBuf` that gets handed to `std::fs`.
//!
//! ### Format coverage
//!
//! Parses what `git diff -p --no-color --no-ext-diff` emits:
//! - Added / Modified / Deleted with hunks (`@@ … @@`)
//! - Renamed and Copied (with `similarity index`)
//! - Mode-only change and mode + content combined
//! - Binary (the "Binary files X and Y differ" line; no body parsed)
//! - `\ No newline at end of file` marker
//!
//! `--raw` and `--numstat` are out of scope — caller chooses `-p`.
//!
//! ### State
//!
//! Two phases per pending file: `Header` (metadata accumulation) and `Hunk`
//! (body line accumulation). A new `diff --git` line flushes the pending
//! file into the result vec and starts a fresh one. The Header → Hunk
//! transition is one-way per file, driven by the first `@@` line.

use crate::error::GitError;
use oximux_core::{
    DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff, LARGE_DIFF_LINE_THRESHOLD,
};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DiffParseError {
    #[error("malformed hunk header at line {line_no}: {raw}")]
    BadHunkHeader { line_no: usize, raw: String },
}

impl From<DiffParseError> for GitError {
    fn from(e: DiffParseError) -> Self {
        GitError::parse(format!("{e}"))
    }
}

/// Parse the stdout of `git diff -p` into a list of file-level diffs.
///
/// Empty input → `Ok(vec![])`. Lines before the first `diff --git` are
/// ignored — `git diff` doesn't emit a preamble, but tolerating one keeps
/// the parser robust against fixture-file noise.
pub fn parse_unified_diff(raw: &str) -> Result<Vec<FileDiff>, DiffParseError> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut phase = Phase::Header;

    for (idx, raw_line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        // CRLF tolerance — macOS git emits LF, but tests may include CRLF.
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(p) = pending.take()
                && let Some(fd) = p.finalize()
            {
                out.push(fd);
            }
            let mut next = Pending::default();
            if let Some((a, b)) = parse_diff_git_paths(rest) {
                next.a_path = Some(a);
                next.b_path = Some(b);
            }
            pending = Some(next);
            phase = Phase::Header;
            continue;
        }

        let Some(p) = pending.as_mut() else {
            continue;
        };

        match phase {
            Phase::Header => {
                if line.starts_with("@@ ") {
                    let hunk = parse_hunk_header(line, line_no)?;
                    p.hunks.push(hunk);
                    phase = Phase::Hunk;
                    continue;
                }
                accumulate_header(p, line);
            }
            Phase::Hunk => {
                if line.starts_with("@@ ") {
                    let hunk = parse_hunk_header(line, line_no)?;
                    p.hunks.push(hunk);
                    continue;
                }
                // `last_mut` is guaranteed Some here because Phase::Hunk is
                // only entered after pushing a hunk.
                if let Some(last) = p.hunks.last_mut()
                    && let Some(diff_line) = parse_hunk_body_line(line)
                {
                    last.lines.push(diff_line);
                }
                // Any other line is silently dropped — real `git diff -p`
                // output never interleaves headers mid-hunk, so this is a
                // pure malformed-input safety net.
            }
        }
    }

    if let Some(p) = pending.take()
        && let Some(fd) = p.finalize()
    {
        out.push(fd);
    }
    Ok(out)
}

#[derive(Copy, Clone)]
enum Phase {
    Header,
    Hunk,
}

#[derive(Default)]
struct Pending {
    // `a/X b/Y` from the `diff --git` line. Used as fallback when explicit
    // rename/copy/--- /+++ headers are absent.
    a_path: Option<String>,
    b_path: Option<String>,
    // From `--- a/X` and `+++ b/Y`. `/dev/null` is recorded as `None`.
    minus_path: Option<String>,
    plus_path: Option<String>,
    // Rename / copy bookkeeping.
    rename_from: Option<String>,
    rename_to: Option<String>,
    copy_from: Option<String>,
    copy_to: Option<String>,
    similarity: Option<u8>,
    // Mode headers.
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    new_file_mode: Option<u32>,
    deleted_file_mode: Option<u32>,
    is_binary: bool,
    hunks: Vec<DiffHunk>,
}

impl Pending {
    fn finalize(self) -> Option<FileDiff> {
        let (path, status) = self.resolve_path_and_status()?;
        let total_lines: usize = self.hunks.iter().map(|h| h.lines.len()).sum();
        // Mode is only meaningful for Added/Deleted (ModeChanged carries
        // its own pair in the status); whichever extended header was
        // present is the right one — they're mutually exclusive in git
        // output.
        let mode = match status {
            DiffStatus::Added => self.new_file_mode,
            DiffStatus::Deleted => self.deleted_file_mode,
            _ => None,
        };
        Some(FileDiff {
            path,
            status,
            hunks: self.hunks,
            large: total_lines > LARGE_DIFF_LINE_THRESHOLD,
            mode,
        })
    }

    fn resolve_path_and_status(&self) -> Option<(PathBuf, DiffStatus)> {
        // FIXME(linux): every `PathBuf::from(<string>)` here assumes the
        // path is valid UTF-8. macOS users get this for free; on Linux a
        // non-UTF-8 filename would already have failed UTF-8 conversion of
        // git's stdout in `Repository::diff_with_args`, so the panic mode
        // is a `GitError::Parse` rather than a silent corruption.
        // Precedence below is from most specific header to least. Rename
        // wins over plain Modified even when hunks are present (the rename
        // is the more informative status); ModeChanged is preferred over
        // Modified when `old mode`/`new mode` headers are present (plan §9
        // open question 2 — accepted as default).
        if self.rename_from.is_some() || self.rename_to.is_some() {
            let from = self.rename_from.clone().or_else(|| self.a_path.clone())?;
            let to = self.rename_to.clone().or_else(|| self.b_path.clone())?;
            return Some((
                PathBuf::from(to),
                DiffStatus::Renamed {
                    from: PathBuf::from(from),
                    similarity: self.similarity.unwrap_or(0),
                },
            ));
        }
        if self.copy_from.is_some() || self.copy_to.is_some() {
            let from = self.copy_from.clone().or_else(|| self.a_path.clone())?;
            let to = self.copy_to.clone().or_else(|| self.b_path.clone())?;
            return Some((
                PathBuf::from(to),
                DiffStatus::Copied {
                    from: PathBuf::from(from),
                    similarity: self.similarity.unwrap_or(0),
                },
            ));
        }
        if self.is_binary {
            let path = self
                .b_path
                .clone()
                .or_else(|| self.a_path.clone())
                .map(PathBuf::from)?;
            return Some((path, DiffStatus::Binary));
        }
        if self.new_file_mode.is_some() {
            let path = self
                .plus_path
                .clone()
                .or_else(|| self.b_path.clone())
                .map(PathBuf::from)?;
            return Some((path, DiffStatus::Added));
        }
        if self.deleted_file_mode.is_some() {
            let path = self
                .minus_path
                .clone()
                .or_else(|| self.a_path.clone())
                .map(PathBuf::from)?;
            return Some((path, DiffStatus::Deleted));
        }
        if let (Some(o), Some(n)) = (self.old_mode, self.new_mode) {
            let path = self
                .plus_path
                .clone()
                .or_else(|| self.b_path.clone())
                .map(PathBuf::from)?;
            return Some((
                path,
                DiffStatus::ModeChanged {
                    old_mode: o,
                    new_mode: n,
                },
            ));
        }
        let path = self
            .plus_path
            .clone()
            .or_else(|| self.b_path.clone())
            .map(PathBuf::from)?;
        Some((path, DiffStatus::Modified))
    }
}

/// Split `"a/X b/Y"` (the suffix after `"diff --git "`) into `(X, Y)`.
///
/// Uses the right-most ` b/` as the separator. For the overwhelmingly
/// common case (no ` b/` substring in either path) this is exact. When a
/// path itself contains ` b/`, this fallback can land off-by-one; callers
/// recover the correct b-path from the subsequent `+++ b/<path>` (or
/// `rename to <path>`) line, which `resolve_path_and_status` prefers over
/// `b_path`. Binary-only and mode-only files lack those headers; users
/// with literal ` b/` in such paths fall outside v1 scope.
fn parse_diff_git_paths(rest: &str) -> Option<(String, String)> {
    // TODO(v2): paths literally containing ` b/` need cross-checking against
    // `+++ b/<path>` / `rename to <path>` / `index` shas to disambiguate the
    // separator unambiguously. Today's rsplit fallback is correct for the
    // 99.9% case; the residue is Binary + mode-only files where no `+++`
    // line exists.
    let stripped = rest.strip_prefix("a/")?;
    let idx = stripped.rfind(" b/")?;
    Some((stripped[..idx].to_string(), stripped[idx + 3..].to_string()))
}

fn accumulate_header(p: &mut Pending, line: &str) {
    if line.starts_with("Binary files ") {
        p.is_binary = true;
    } else if let Some(rest) = line.strip_prefix("--- ") {
        if rest != "/dev/null" {
            p.minus_path = Some(strip_a_prefix(rest));
        }
    } else if let Some(rest) = line.strip_prefix("+++ ") {
        if rest != "/dev/null" {
            p.plus_path = Some(strip_b_prefix(rest));
        }
    } else if let Some(rest) = line.strip_prefix("rename from ") {
        p.rename_from = Some(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("rename to ") {
        p.rename_to = Some(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("copy from ") {
        p.copy_from = Some(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("copy to ") {
        p.copy_to = Some(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("similarity index ") {
        if let Some(num) = rest.strip_suffix('%')
            && let Ok(n) = num.parse::<u8>()
        {
            p.similarity = Some(n);
        }
    } else if let Some(rest) = line.strip_prefix("old mode ")
        // Git prints modes as `%06o` — parse the digits as octal so the
        // stored value is the actual POSIX mode (e.g. "100644" → 0o100644).
        && let Ok(n) = u32::from_str_radix(rest.trim(), 8)
    {
        p.old_mode = Some(n);
    } else if let Some(rest) = line.strip_prefix("new mode ")
        && let Ok(n) = u32::from_str_radix(rest.trim(), 8)
    {
        p.new_mode = Some(n);
    } else if let Some(rest) = line.strip_prefix("new file mode ")
        && let Ok(n) = u32::from_str_radix(rest.trim(), 8)
    {
        p.new_file_mode = Some(n);
    } else if let Some(rest) = line.strip_prefix("deleted file mode ")
        && let Ok(n) = u32::from_str_radix(rest.trim(), 8)
    {
        p.deleted_file_mode = Some(n);
    }
    // index, dissimilarity index, GIT binary patch lines etc. fall through.
}

fn strip_a_prefix(s: &str) -> String {
    let no_tab = trim_legacy_timestamp(s);
    no_tab.strip_prefix("a/").unwrap_or(no_tab).to_string()
}

fn strip_b_prefix(s: &str) -> String {
    let no_tab = trim_legacy_timestamp(s);
    no_tab.strip_prefix("b/").unwrap_or(no_tab).to_string()
}

/// Strip the historical tab-separated timestamp suffix that `git diff` emits
/// on `---`/`+++` lines when the path contains spaces (legacy unified-diff
/// convention from `diff(1)`). Without this, a path like `"my file.txt"`
/// round-trips as `"my file.txt\t<timestamp>"`.
fn trim_legacy_timestamp(s: &str) -> &str {
    match s.find('\t') {
        Some(i) => &s[..i],
        None => s,
    }
}

fn parse_hunk_body_line(line: &str) -> Option<DiffLine> {
    // Empty line in a hunk body is a context line of zero width — git emits
    // these for blank lines in the source. They lose their leading ' ' to
    // shell-pipe trimming sometimes, so accept either.
    if line.is_empty() {
        return Some(DiffLine {
            kind: DiffLineKind::Context,
            content: String::new(),
        });
    }
    let (kind, content) = match line.as_bytes()[0] {
        b' ' => (DiffLineKind::Context, &line[1..]),
        b'+' => (DiffLineKind::Added, &line[1..]),
        b'-' => (DiffLineKind::Removed, &line[1..]),
        b'\\' => (DiffLineKind::NoNewlineHint, line[1..].trim_start()),
        _ => return None,
    };
    Some(DiffLine {
        kind,
        content: content.to_string(),
    })
}

fn parse_hunk_header(line: &str, line_no: usize) -> Result<DiffHunk, DiffParseError> {
    let bail = || DiffParseError::BadHunkHeader {
        line_no,
        raw: line.to_string(),
    };
    // Body is everything after the leading "@@ ".
    let body = line.strip_prefix("@@ ").ok_or_else(bail)?;
    // Closing "@@" sits after the range spec. Suffix (if any) lives past
    // that — git uses it for the enclosing function name etc.
    let close = body.find(" @@").ok_or_else(bail)?;
    let range = &body[..close];
    let suffix = body[close + 3..].trim_start().to_string();

    let mut parts = range.splitn(2, ' ');
    let old = parts.next().ok_or_else(bail)?;
    let new = parts.next().ok_or_else(bail)?;
    let (old_start, old_lines) =
        parse_range_spec(old.strip_prefix('-').ok_or_else(bail)?).ok_or_else(bail)?;
    let (new_start, new_lines) =
        parse_range_spec(new.strip_prefix('+').ok_or_else(bail)?).ok_or_else(bail)?;

    Ok(DiffHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        header_suffix: suffix,
        lines: Vec::new(),
    })
}

/// `"12,3"` → `(12, 3)`. `"12"` → `(12, 1)` — git omits the line count
/// when it's exactly one.
fn parse_range_spec(s: &str) -> Option<(u32, u32)> {
    if let Some((a, b)) = s.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        Some((s.parse().ok()?, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty() {
        assert!(parse_unified_diff("").unwrap().is_empty());
    }

    #[test]
    fn parse_diff_git_paths_basic() {
        assert_eq!(
            parse_diff_git_paths("a/foo.rs b/foo.rs"),
            Some(("foo.rs".to_string(), "foo.rs".to_string()))
        );
    }

    #[test]
    fn parse_diff_git_paths_paths_with_spaces() {
        assert_eq!(
            parse_diff_git_paths("a/some dir/file.rs b/some dir/file.rs"),
            Some((
                "some dir/file.rs".to_string(),
                "some dir/file.rs".to_string()
            ))
        );
    }

    #[test]
    fn parse_range_spec_single_line() {
        assert_eq!(parse_range_spec("42"), Some((42, 1)));
        assert_eq!(parse_range_spec("42,5"), Some((42, 5)));
        assert_eq!(parse_range_spec("0,0"), Some((0, 0)));
    }

    #[test]
    fn parse_hunk_header_with_suffix() {
        let h = parse_hunk_header("@@ -10,3 +12,4 @@ fn main() {", 1).unwrap();
        assert_eq!(h.old_start, 10);
        assert_eq!(h.old_lines, 3);
        assert_eq!(h.new_start, 12);
        assert_eq!(h.new_lines, 4);
        assert_eq!(h.header_suffix, "fn main() {");
    }

    #[test]
    fn parse_hunk_header_no_suffix() {
        let h = parse_hunk_header("@@ -1 +1 @@", 1).unwrap();
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_lines, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_lines, 1);
        assert_eq!(h.header_suffix, "");
    }

    #[test]
    fn parse_hunk_header_malformed_errors() {
        let err = parse_hunk_header("@@ bogus", 7).unwrap_err();
        match err {
            DiffParseError::BadHunkHeader { line_no, .. } => assert_eq!(line_no, 7),
        }
    }
}
