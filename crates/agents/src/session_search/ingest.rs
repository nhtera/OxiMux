//! Transcript discovery and the per-file read: stream lines from the
//! watermark, shape them into rows, and commit in bounded batches.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use rusqlite::Connection;

use super::turn::{MetaDelta, Row, Turn, shape};
use super::watermark::{ReadPlan, Stat, Watermark};
use super::writer::{self, Batch};
use super::{Agent, IndexSources, parse_claude, parse_codex};

/// One transcript file found on disk.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub agent: Agent,
    pub stat: Stat,
}

impl SourceFile {
    pub fn key(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

/// What one discovery pass found.
pub struct Discovered {
    /// Every transcript under the configured roots, newest first.
    pub files: Vec<SourceFile>,
    /// Every root was either listed or is absent. `false` after a transient
    /// failure (permissions, I/O) — then a missing file proves nothing, and
    /// retiring would wipe and later re-index a whole root.
    pub complete: bool,
}

pub fn discover(sources: &IndexSources) -> Discovered {
    let mut files = Vec::new();
    let mut complete = true;
    if let Some(dir) = &sources.claude_dir {
        complete &= discover_claude(dir, &mut files);
    }
    if let Some(dir) = &sources.codex_dir {
        let mut found = Vec::new();
        complete &= collect_rollouts(&dir.join("sessions"), 0, &mut found);
        files.extend(found.into_iter().filter_map(|p| source(p, Agent::Codex, &mut complete)));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.stat.mtime_ms));
    Discovered { files, complete }
}

/// Every `rollout-*.jsonl` under Codex's `sessions/YYYY/MM/DD` tree
/// (depth-bounded, symlinks never followed). `false` when any directory in it
/// failed to list for a reason other than not existing.
fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> bool {
    const MAX_DEPTH: usize = 5;
    if depth > MAX_DEPTH {
        return true;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    let mut complete = true;
    for entry in entries {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        let Ok(ft) = entry.file_type() else {
            complete = false;
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            complete &= collect_rollouts(&path, depth + 1, out);
        } else if ft.is_file()
            && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        {
            out.push(path);
        }
    }
    complete
}

/// `<claude_dir>/projects/<slug>/<session>.jsonl` — one level only, so
/// subagent transcripts (`<session>/subagents/…`) stay out. Symlinks are
/// never followed.
fn discover_claude(claude_dir: &Path, out: &mut Vec<SourceFile>) -> bool {
    let root = claude_dir.join("projects");
    let projects = match fs::read_dir(&root) {
        Ok(p) => p,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    let mut complete = true;
    for proj in projects.flatten() {
        if !proj.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
            continue;
        }
        let Ok(files) = fs::read_dir(proj.path()) else {
            complete = false;
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "jsonl") && f.file_type().is_ok_and(|t| t.is_file()) {
                out.extend(source(path, Agent::Claude, &mut complete));
            }
        }
    }
    complete
}

/// Stat a listed file. A file gone since the listing is simply absent; any
/// other failure clears `complete`, so a transient error never retires the
/// file's indexed content.
fn source(path: PathBuf, agent: Agent, complete: &mut bool) -> Option<SourceFile> {
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            *complete &= e.kind() == std::io::ErrorKind::NotFound;
            return None;
        }
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    Some(SourceFile { path, agent, stat: Stat { mtime_ms, size: meta.len() } })
}

/// Is the byte just before `offset` a newline? (The cheap "head unchanged" check.)
pub fn newline_before(path: &Path, offset: u64) -> bool {
    let Ok(mut f) = fs::File::open(path) else { return false };
    let mut b = [0u8; 1];
    offset > 0 && f.seek(SeekFrom::Start(offset - 1)).is_ok() && f.read_exact(&mut b).is_ok() && b[0] == b'\n'
}

/// How a file read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEnd {
    /// Read to the last complete line.
    Done,
    /// Stopped early (budget or stop flag); the watermark marks where.
    Paused,
}

/// Text characters per committed batch — bounds memory and commit time on
/// huge transcripts.
const BATCH_CHARS: usize = 1_000_000;
/// Budget/stop checks run every this many lines, or this many bytes read
/// (one line can be megabytes: an inlined screenshot).
const CHECK_EVERY: usize = 256;
const CHECK_BYTES: u64 = 4 * 1024 * 1024;

/// Read `file` per `plan` into the index. Lines are only consumed through
/// their trailing `\n`, so a line still being written is picked up whole on a
/// later pass.
pub fn index_file(
    conn: &mut Connection,
    file: &SourceFile,
    plan: ReadPlan,
    prev: Option<&Watermark>,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<ReadEnd> {
    let key = file.key();
    // Open before touching the index: a file that cannot be read must keep
    // its watermark (and failure count) rather than lose it to the Replace.
    let handle = fs::File::open(&file.path)?;
    let (offset, mut session_row_id) = match plan {
        ReadPlan::Skip | ReadPlan::HeldOut => return Ok(ReadEnd::Done),
        ReadPlan::Append { offset } => (offset, prev.and_then(|p| p.session_row_id)),
        ReadPlan::Replace => {
            if prev.is_some() {
                writer::drop_content(conn, &key)?;
            }
            (0, None)
        }
    };
    let fallback_id = fallback_session_id(file);
    let mut reader = BufReader::new(handle);
    reader.seek(SeekFrom::Start(offset))?;
    let parse = match file.agent {
        Agent::Claude => parse_claude::parse_line,
        Agent::Codex => parse_codex::parse_line,
    };
    let mut meta = MetaDelta::default();
    let mut turns: Vec<Turn> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut chars = 0usize;
    let mut buf = Vec::new();
    let mut lines = 0usize;
    let mut consumed = offset;
    let mut checked_at = offset;
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        consumed += n as u64;
        parse(String::from_utf8_lossy(&buf).trim_end(), &mut meta, &mut turns);
        for row in turns.drain(..).flat_map(shape) {
            chars += row.text.len();
            rows.push(row);
        }
        lines += 1;
        let due = lines.is_multiple_of(CHECK_EVERY) || consumed - checked_at >= CHECK_BYTES;
        if due {
            checked_at = consumed;
        }
        let paused = due && (stop.load(Ordering::Relaxed) || Instant::now() >= deadline);
        if chars >= BATCH_CHARS || paused {
            // Mid-file: record the consumed offset as the size, so a read that
            // never reaches the final commit (pause, crash, quit) leaves the
            // file looking changed and the next pass resumes it.
            session_row_id = commit(conn, file, &key, &fallback_id, session_row_id, &meta, &rows, consumed, consumed)?;
            meta = MetaDelta::default();
            rows.clear();
            chars = 0;
            if paused {
                return Ok(ReadEnd::Paused);
            }
        }
    }
    // Read to the last complete line: record the stat size, so an unchanged
    // file (even one ending in a partial line) is skipped next pass.
    commit(conn, file, &key, &fallback_id, session_row_id, &meta, &rows, consumed, file.stat.size.max(consumed))?;
    Ok(ReadEnd::Done)
}

#[allow(clippy::too_many_arguments)]
fn commit(
    conn: &mut Connection,
    file: &SourceFile,
    key: &str,
    fallback_id: &str,
    session_row_id: Option<i64>,
    meta: &MetaDelta,
    rows: &[Row],
    offset: u64,
    size: u64,
) -> Result<Option<i64>> {
    writer::commit_batch(
        conn,
        &Batch {
            path: key,
            agent: file.agent,
            fallback_session_id: fallback_id,
            session_row_id,
            meta,
            rows,
            offset,
            mtime_ms: file.stat.mtime_ms,
            size,
        },
    )
}

/// The id a transcript goes by when it does not name one: Claude's file stem
/// is the session id; a Codex rollout ends in `-<uuid>.jsonl`.
fn fallback_session_id(file: &SourceFile) -> String {
    let stem = file.path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    match file.agent {
        Agent::Claude => stem.to_string(),
        Agent::Codex => stem.len().checked_sub(36).and_then(|i| stem.get(i..)).unwrap_or(stem).to_string(),
    }
}
