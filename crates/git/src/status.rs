//! Parser for `git status --porcelain=v2 --branch -z`.
//!
//! Format reference: `man git-status` → "Porcelain Format Version 2".
//!
//! With `-z`, every record (header or entry) is terminated by NUL instead of LF.
//! Type-2 (rename/copy) is emitted as **two consecutive NUL-terminated chunks**
//! — the entry record itself (ending at the new path's NUL) followed by a
//! standalone NUL-terminated chunk for the orig path. The parser consumes the
//! orig-path chunk with a second `RecordIter::next_record()` call after seeing
//! a `b'2'` discriminator. Same convention applies to any future `-z`-mode
//! command (`diff --name-status -z`, `ls-files -z`, etc.).
//!
//! Header lines start with `# ` and carry branch metadata:
//!   `# branch.oid <oid>|(initial)`
//!   `# branch.head <name>|(detached)`
//!   `# branch.upstream <ref>`            (optional)
//!   `# branch.ab +<ahead> -<behind>`     (optional)
//!
//! Entry lines:
//!   `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`                    ordinary
//!   `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>\0<orig>`   rename/copy
//!   `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`          unmerged
//!   `? <path>`                                                         untracked
//!   `! <path>`                                                         ignored

use crate::error::{GitError, Result};
use oximux_core::{FileStatus, GitState, IndexStatus, RenameInfo, RenameKind, WorktreeStatus};
use std::path::PathBuf;

pub fn parse_porcelain_v2(buf: &[u8]) -> Result<GitState> {
    let mut state = GitState::default();
    let mut iter = RecordIter::new(buf);

    while let Some(rec) = iter.next_record() {
        if rec.is_empty() {
            continue;
        }
        match rec[0] {
            b'#' => parse_header(rec, &mut state)?,
            b'1' => parse_ordinary(rec, &mut state)?,
            b'2' => {
                // type-2 has one extra NUL-delimited field (orig path).
                let orig = iter
                    .next_record()
                    .ok_or_else(|| GitError::parse("type-2 record missing orig path"))?;
                parse_renamed(rec, orig, &mut state)?;
            }
            b'u' => parse_unmerged(rec, &mut state)?,
            b'?' => parse_path_only(
                rec,
                IndexStatus::Untracked,
                WorktreeStatus::Untracked,
                &mut state,
            )?,
            b'!' => parse_path_only(
                rec,
                IndexStatus::Ignored,
                WorktreeStatus::Ignored,
                &mut state,
            )?,
            other => {
                return Err(GitError::parse(format!(
                    "unknown porcelain v2 record type: {:?}",
                    other as char
                )));
            }
        }
    }
    Ok(state)
}

/// Splits a NUL-terminated byte stream into records. `next_record` returns one
/// slice per NUL-delimited chunk; trailing data without a NUL is also yielded.
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

fn parse_header(rec: &[u8], state: &mut GitState) -> Result<()> {
    // rec = b"# branch.head main"
    let s =
        std::str::from_utf8(rec).map_err(|e| GitError::parse(format!("header not utf-8: {e}")))?;
    let s = s.strip_prefix("# ").unwrap_or(s);
    let mut parts = s.splitn(2, ' ');
    let key = parts.next().unwrap_or("");
    let val = parts.next().unwrap_or("").trim();
    match key {
        "branch.oid" => {
            state.head_oid = if val == "(initial)" {
                None
            } else {
                Some(val.to_string())
            };
        }
        "branch.head" => {
            state.branch = if val == "(detached)" {
                None
            } else {
                Some(val.to_string())
            };
        }
        "branch.upstream" => {
            state.upstream = Some(val.to_string());
        }
        "branch.ab" => {
            // val = "+12 -3"
            let mut tokens = val.split_whitespace();
            let ahead_tok = tokens
                .next()
                .ok_or_else(|| GitError::parse("branch.ab missing ahead"))?;
            let behind_tok = tokens
                .next()
                .ok_or_else(|| GitError::parse("branch.ab missing behind"))?;
            state.ahead = ahead_tok
                .strip_prefix('+')
                .unwrap_or(ahead_tok)
                .parse()
                .map_err(|e| GitError::parse(format!("branch.ab ahead: {e}")))?;
            state.behind = behind_tok
                .strip_prefix('-')
                .unwrap_or(behind_tok)
                .parse()
                .map_err(|e| GitError::parse(format!("branch.ab behind: {e}")))?;
        }
        _ => {
            // Unknown header — git may add new ones; ignore rather than fail.
        }
    }
    Ok(())
}

// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
fn parse_ordinary(rec: &[u8], state: &mut GitState) -> Result<()> {
    let fields = split_n_fields_then_path(rec, 8)?;
    let xy = fields.fixed[1];
    if xy.len() != 2 {
        return Err(GitError::parse(format!("ordinary XY not 2 chars: {xy:?}")));
    }
    let (index, worktree) = xy_to_status(xy.as_bytes()[0], xy.as_bytes()[1])?;
    state.files.push(FileStatus {
        path: PathBuf::from(fields.path),
        index,
        worktree,
        rename: None,
    });
    Ok(())
}

// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>` + NUL + orig
fn parse_renamed(rec: &[u8], orig: &[u8], state: &mut GitState) -> Result<()> {
    let fields = split_n_fields_then_path(rec, 9)?;
    let xy = fields.fixed[1];
    if xy.len() != 2 {
        return Err(GitError::parse(format!("renamed XY not 2 chars: {xy:?}")));
    }
    let (index, worktree) = xy_to_status(xy.as_bytes()[0], xy.as_bytes()[1])?;
    let xscore = fields.fixed[8];
    let (kind, score) = parse_rename_score(xscore)?;
    let orig_str = std::str::from_utf8(orig)
        .map_err(|e| GitError::parse(format!("orig path not utf-8: {e}")))?;
    state.files.push(FileStatus {
        path: PathBuf::from(fields.path),
        index,
        worktree,
        rename: Some(RenameInfo {
            orig_path: PathBuf::from(orig_str),
            kind,
            score,
        }),
    });
    Ok(())
}

// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
fn parse_unmerged(rec: &[u8], state: &mut GitState) -> Result<()> {
    let fields = split_n_fields_then_path(rec, 10)?;
    let xy = fields.fixed[1];
    if xy.len() != 2 {
        return Err(GitError::parse(format!("unmerged XY not 2 chars: {xy:?}")));
    }
    // For unmerged, X and Y are both conflict codes; flag both sides Unmerged.
    state.files.push(FileStatus {
        path: PathBuf::from(fields.path),
        index: IndexStatus::Unmerged,
        worktree: WorktreeStatus::Unmerged,
        rename: None,
    });
    Ok(())
}

// `? <path>` or `! <path>` — single space, then everything is path.
fn parse_path_only(
    rec: &[u8],
    index: IndexStatus,
    worktree: WorktreeStatus,
    state: &mut GitState,
) -> Result<()> {
    if rec.len() < 2 || rec[1] != b' ' {
        return Err(GitError::parse("malformed ?/! record"));
    }
    let path = std::str::from_utf8(&rec[2..])
        .map_err(|e| GitError::parse(format!("path not utf-8: {e}")))?;
    state.files.push(FileStatus {
        path: PathBuf::from(path),
        index,
        worktree,
        rename: None,
    });
    Ok(())
}

struct Split<'a> {
    fixed: Vec<&'a str>,
    path: &'a str,
}

/// Split the first `n` space-separated fields, treating the rest of the record
/// as the final path field. Path may contain spaces (NUL is the terminator with
/// `-z`, so spaces in filenames are fine). Strict single-space delimiter:
/// porcelain v2 never emits double spaces between fields, and being liberal
/// silently shifts field indices when a malformed record sneaks through.
fn split_n_fields_then_path(rec: &[u8], n: usize) -> Result<Split<'_>> {
    let s =
        std::str::from_utf8(rec).map_err(|e| GitError::parse(format!("record not utf-8: {e}")))?;
    let mut parts: Vec<&str> = s.splitn(n + 1, ' ').collect();
    if parts.len() < n + 1 {
        return Err(GitError::parse(format!(
            "expected {n} fields + path, got {} parts",
            parts.len()
        )));
    }
    let path = parts.pop().expect("checked len above");
    // splitn surfaces adjacent separators as empty fields — reject so we never
    // index past the intended field slot.
    if parts.iter().any(|p| p.is_empty()) {
        return Err(GitError::parse("empty field (double space?) in record"));
    }
    Ok(Split { fixed: parts, path })
}

fn parse_rename_score(s: &str) -> Result<(RenameKind, u8)> {
    let kind = match s.as_bytes().first() {
        Some(b'R') => RenameKind::Rename,
        Some(b'C') => RenameKind::Copy,
        _ => return Err(GitError::parse(format!("bad rename score: {s:?}"))),
    };
    let n: u16 = s[1..]
        .parse()
        .map_err(|e| GitError::parse(format!("rename score: {e}")))?;
    Ok((kind, n.min(100) as u8))
}

fn xy_to_status(x: u8, y: u8) -> Result<(IndexStatus, WorktreeStatus)> {
    Ok((index_code(x)?, worktree_code(y)?))
}

fn index_code(c: u8) -> Result<IndexStatus> {
    Ok(match c {
        b'.' => IndexStatus::Unmodified,
        b'M' => IndexStatus::Modified,
        b'A' => IndexStatus::Added,
        b'D' => IndexStatus::Deleted,
        b'R' => IndexStatus::Renamed,
        b'C' => IndexStatus::Copied,
        b'?' => IndexStatus::Untracked,
        b'!' => IndexStatus::Ignored,
        b'U' => IndexStatus::Unmerged,
        b'T' => IndexStatus::Modified, // type change ~ modified
        other => {
            return Err(GitError::parse(format!(
                "unknown index code: {:?}",
                other as char
            )));
        }
    })
}

fn worktree_code(c: u8) -> Result<WorktreeStatus> {
    Ok(match c {
        b'.' => WorktreeStatus::Unmodified,
        b'M' => WorktreeStatus::Modified,
        b'D' => WorktreeStatus::Deleted,
        b'R' => WorktreeStatus::Renamed,
        b'?' => WorktreeStatus::Untracked,
        b'!' => WorktreeStatus::Ignored,
        b'U' => WorktreeStatus::Unmerged,
        b'T' => WorktreeStatus::Modified,
        b'A' => WorktreeStatus::Modified, // shouldn't appear on Y in v2, be liberal
        other => {
            return Err(GitError::parse(format!(
                "unknown worktree code: {:?}",
                other as char
            )));
        }
    })
}
