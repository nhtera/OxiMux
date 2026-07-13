//! Primary-CLI session-log access (shared plumbing).
//!
//! The primary agent CLI journals every session as line-delimited JSON
//! under `<home>/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`. Two
//! consumers read those files, both background-only:
//!
//! - [`activity`] — tail the newest log of a Running session for the
//!   current tool call ("Bash: cargo test…") shown on dashboard rows.
//! - [`usage`] — tally per-message token usage into rate-limit-window
//!   estimates for the status-bar meter.
//!
//! This module owns the shared pieces: cwd→slug mapping, newest-log
//! discovery, bounded tail reads, and timestamp parsing. Everything
//! degrades by returning `None` — log format drift must never crash
//! (phase-2 discipline); consumers hide their UI when data is absent.

pub mod activity;
pub mod ansi_strip;
pub mod import_provider_index;
pub mod session_index;
pub mod session_preview;
pub mod usage;
pub mod usage_oauth;
pub mod usage_probe;

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Map a launch cwd to the CLI's per-project log directory name: every
/// non-alphanumeric byte becomes `-` (so `/a/b.c` → `-a-b-c`), matching
/// the observed on-disk layout.
pub fn cwd_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The log directory for a launch cwd: `<claude_dir>/projects/<slug>`.
/// `claude_dir` is the CLI's state root (normally `~/.claude`).
pub fn project_log_dir(claude_dir: &Path, cwd: &Path) -> PathBuf {
    claude_dir.join("projects").join(cwd_slug(cwd))
}

/// Newest (by mtime) `.jsonl` session log in `dir`, with its mtime in unix
/// millis. `None` when the directory is missing/empty — the common case
/// for workspaces that never ran the primary CLI.
pub fn newest_session_log(dir: &Path) -> Option<(PathBuf, i64)> {
    let entries = fs::read_dir(dir).ok()?;
    let mut best: Option<(PathBuf, i64)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = unix_ms(meta.modified().ok()?);
        if best.as_ref().is_none_or(|(_, m)| mtime > *m) {
            best = Some((path, mtime));
        }
    }
    best
}

/// Read at most the trailing `max_bytes` of `path` as a lossy string.
/// Bounded so tailing a multi-hundred-MB session log costs one small read.
pub fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Parse the log's RFC 3339 timestamps (`2026-06-12T07:01:02.345Z`) to
/// unix millis. `None` on any drift from that format.
pub fn parse_timestamp_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn unix_ms(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Current wall clock in unix millis. Callers pass this into the pure
/// parsing/window functions so tests can drive synthetic time.
pub fn now_unix_ms() -> i64 {
    unix_ms(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_slug_maps_separators_and_dots_to_dashes() {
        assert_eq!(
            cwd_slug(Path::new("/Users/jane/Code/my.app_v2")),
            "-Users-jane-Code-my-app-v2"
        );
    }

    #[test]
    fn cwd_slug_keeps_existing_dashes() {
        assert_eq!(cwd_slug(Path::new("/a/b-c")), "-a-b-c");
    }

    #[test]
    fn project_log_dir_joins_projects_and_slug() {
        let dir = project_log_dir(Path::new("/home/u/.claude"), Path::new("/w/x"));
        assert_eq!(dir, PathBuf::from("/home/u/.claude/projects/-w-x"));
    }

    #[test]
    fn parse_timestamp_ms_accepts_rfc3339_zulu() {
        let ms = parse_timestamp_ms("2026-06-12T00:00:00.000Z").unwrap();
        assert_eq!(ms, 1781222400000);
    }

    #[test]
    fn parse_timestamp_ms_rejects_garbage() {
        assert!(parse_timestamp_ms("yesterday-ish").is_none());
        assert!(parse_timestamp_ms("").is_none());
    }

    #[test]
    fn newest_session_log_picks_latest_mtime_and_skips_non_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jsonl");
        let new = dir.path().join("new.jsonl");
        let noise = dir.path().join("notes.txt");
        std::fs::write(&old, "{}\n").unwrap();
        std::fs::write(&noise, "x").unwrap();
        // Ensure a strictly newer mtime on `new`.
        std::fs::write(&new, "{}\n").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&new).unwrap();
        f.set_modified(newer).unwrap();
        let (path, _) = newest_session_log(dir.path()).unwrap();
        assert_eq!(path, new);
    }

    #[test]
    fn newest_session_log_missing_dir_is_none() {
        assert!(newest_session_log(Path::new("/nonexistent/xyz")).is_none());
    }

    #[test]
    fn read_tail_bounds_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\ncccc\n").unwrap();
        let tail = read_tail(&p, 6).unwrap();
        assert_eq!(tail, "\ncccc\n");
        let all = read_tail(&p, 1024).unwrap();
        assert_eq!(all, "aaaa\nbbbb\ncccc\n");
    }
}
