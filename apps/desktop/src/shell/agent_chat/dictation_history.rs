//! On-disk store for recent dictation transcripts (the Voice-pane "History").
//!
//! One JSONL file in the app data dir (next to `dictation.toml` + the model
//! store), newest-first, capped to [`HISTORY_CAP`] entries. Every completed
//! dictation — the `Final` event, from any pane — appends one line via
//! [`record`]; the Voice pane reads [`entries`] to render its history card, and
//! [`clear`] wipes the file. Kept dependency-light: `record` reads the head,
//! prepends, and rewrites (the cap keeps this trivially small).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Same per-user data dir as `dictation.toml` + the model store.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const HISTORY_FILE: &str = "dictation-history.jsonl";
/// How many transcripts to keep. Older entries fall off the end.
pub const HISTORY_CAP: usize = 50;

/// One recorded transcript: a unix-seconds timestamp + the text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix seconds when the transcript finalized.
    pub ts: i64,
    pub text: String,
}

fn history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR).join(HISTORY_FILE))
}

/// All history entries, newest first. Best-effort: a missing/unreadable file
/// yields an empty list, and individual corrupt lines are skipped.
pub fn entries() -> Vec<HistoryEntry> {
    history_path().map(|p| read_from(&p)).unwrap_or_default()
}

/// Prepend `text` (trimmed) as the newest entry, stamped `now` (unix seconds),
/// capped at [`HISTORY_CAP`]. Blank text is ignored. Best-effort: I/O errors are
/// logged, never surfaced (history is a convenience, not a correctness surface).
pub fn record(text: &str) {
    let ts = chrono::Local::now().timestamp();
    if let Some(path) = history_path() {
        record_to(&path, ts, text);
    }
}

/// Remove all history.
pub fn clear() {
    if let Some(path) = history_path() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Format an entry's timestamp for display, e.g. `Jul 16, 11:27 PM`, in the
/// local timezone. Falls back to the raw seconds if the value is out of range.
pub fn format_ts(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.format("%b %-d, %-I:%M %p").to_string(),
        None => ts.to_string(),
    }
}

fn read_from(path: &Path) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .collect()
}

fn record_to(path: &Path, ts: i64, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let mut items = read_from(path);
    items.insert(
        0,
        HistoryEntry {
            ts,
            text: text.to_string(),
        },
    );
    items.truncate(HISTORY_CAP);
    write_all(path, &items);
}

fn write_all(path: &Path, items: &[HistoryEntry]) {
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(%e, "dictation history: create dir failed");
        return;
    }
    let mut buf = String::new();
    for item in items {
        match serde_json::to_string(item) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            Err(e) => tracing::warn!(%e, "dictation history: serialize failed"),
        }
    }
    if let Err(e) = std::fs::write(path, buf) {
        tracing::warn!(%e, "dictation history: write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oximux-dict-hist-{name}.jsonl"))
    }

    #[test]
    fn record_prepends_newest_first() {
        let path = tmp_path("order");
        let _ = std::fs::remove_file(&path);
        record_to(&path, 100, "first");
        record_to(&path, 200, "second");
        let items = read_from(&path);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "second", "newest is at the front");
        assert_eq!(items[1].text, "first");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_caps_and_drops_oldest() {
        let path = tmp_path("cap");
        let _ = std::fs::remove_file(&path);
        for i in 0..(HISTORY_CAP + 5) {
            record_to(&path, i as i64, &format!("line {i}"));
        }
        let items = read_from(&path);
        assert_eq!(items.len(), HISTORY_CAP, "capped");
        // The newest (highest i) is first; the oldest kept is #5 (0..4 dropped).
        assert_eq!(items[0].text, format!("line {}", HISTORY_CAP + 4));
        assert_eq!(items.last().unwrap().text, "line 5");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blank_text_is_ignored() {
        let path = tmp_path("blank");
        let _ = std::fs::remove_file(&path);
        record_to(&path, 1, "   ");
        assert!(read_from(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn text_is_trimmed() {
        let path = tmp_path("trim");
        let _ = std::fs::remove_file(&path);
        record_to(&path, 1, "  hello world  ");
        assert_eq!(read_from(&path)[0].text, "hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "not json\n{\"ts\":5,\"text\":\"ok\"}\n").unwrap();
        let items = read_from(&path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "ok");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(read_from(&tmp_path("does-not-exist-xyz")).is_empty());
    }
}
