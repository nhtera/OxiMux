//! Read-only index of past agent sessions for a history/resume picker.
//!
//! Two on-disk sources, both journaled by the agent CLIs themselves:
//!
//! - Claude Code — one `.jsonl` per session under
//!   `<claude_dir>/projects/<cwd-slug>/<session-uuid>.jsonl` (the same files
//!   [`super::activity`] tails). The file stem IS the session id; the first
//!   `user` entry is the opening prompt; entries carry `cwd` + `timestamp`.
//! - Codex — a compact `<codex_dir>/session_index.jsonl`, one
//!   `{id, thread_name, updated_at}` object per line.
//!
//! Everything degrades to "absent": unreadable dirs, malformed lines, and
//! format drift yield fewer entries, never a crash. Reads are bounded — a
//! multi-hundred-MB session log costs a small head + tail read, not a full
//! slurp — so building the index over a busy `~/.claude` stays cheap.
//!
//! The build is synchronous IO; callers run it on a background executor
//! (mirroring how `activity`/`usage` are consumed) and never on the UI
//! thread.

use std::fs;
use std::path::Path;

use serde_json::Value;

use oximux_core::AgentAdapter;

use super::{parse_timestamp_ms, read_tail};

/// One past session, enough to list, sort, search, and resume/fork it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// CLI session identifier (Claude: log file stem; Codex: index `id`).
    pub session_id: String,
    pub adapter: AgentAdapter,
    /// Launch directory, when the log records one (Codex index omits it).
    pub cwd: Option<String>,
    /// Row title: Claude's own `lastPrompt`, else the first user message with
    /// slash-command XML unwrapped (Codex: thread name). One line, capped.
    pub title: Option<String>,
    /// Git branch the session ran on (Claude `gitBranch`; absent for Codex).
    pub git_branch: Option<String>,
    /// Newest entry timestamp in unix millis; the listing sort key.
    pub last_message_ts_ms: Option<i64>,
    /// On-disk log size in bytes (Claude only); shown in the row subtitle.
    pub size_bytes: Option<u64>,
    /// Journal line count — exact when the whole log was parsed, `None` when
    /// the log was too large and only its head/tail were read.
    pub entry_count: Option<usize>,
}

/// Logs at or below this size are parsed whole (so `entry_count` is exact);
/// larger ones fall back to bounded head + tail reads.
const FULL_PARSE_LIMIT: u64 = 1024 * 1024;
/// Head window for a large log: the opening prompt + cwd live in the first
/// few lines.
const HEAD_BYTES: u64 = 16 * 1024;
/// Tail window for a large log: the newest timestamp is at the very end.
const TAIL_BYTES: u64 = 16 * 1024;
/// One-line prompt preview cap (characters, ellipsis appended past it).
const PROMPT_MAX_CHARS: usize = 200;

/// Build the merged, newest-first session index from the two CLI state
/// roots (normally `~/.claude` and `~/.codex`). Missing roots contribute
/// nothing.
pub struct SessionIndex;

impl SessionIndex {
    pub fn build(claude_dir: &Path, codex_dir: &Path) -> Vec<SessionEntry> {
        let mut entries = Vec::new();
        collect_claude(claude_dir, &mut entries);
        collect_codex(codex_dir, &mut entries);
        // Newest first; entries without a timestamp sort to the bottom.
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_message_ts_ms));
        entries
    }
}

// --- Claude Code -----------------------------------------------------------

fn collect_claude(claude_dir: &Path, out: &mut Vec<SessionEntry>) {
    let projects = claude_dir.join("projects");
    let Ok(project_dirs) = fs::read_dir(&projects) else {
        return;
    };
    for proj in project_dirs.flatten() {
        // Skip symlinked project dirs — never follow links out of the root.
        if !proj.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(proj.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if f.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(entry) = build_claude_entry(stem, &path) {
                out.push(entry);
            }
        }
    }
}

fn build_claude_entry(session_id: &str, path: &Path) -> Option<SessionEntry> {
    let len = fs::metadata(path).ok()?.len();
    if len <= FULL_PARSE_LIMIT {
        let content = fs::read_to_string(path).ok()?;
        let mut entry = parse_claude_jsonl(session_id, &content)?;
        entry.size_bytes = Some(len);
        return Some(entry);
    }
    // Large log: title + cwd + branch from the head, newest ts from the tail.
    let head = read_head(path, HEAD_BYTES).unwrap_or_default();
    let tail = read_tail(path, TAIL_BYTES).unwrap_or_default();
    let title = claude_title(&head).or_else(|| claude_title(&tail));
    let cwd = claude_cwd(&head).or_else(|| claude_cwd(&tail));
    let git_branch = claude_git_branch(&head).or_else(|| claude_git_branch(&tail));
    let last_message_ts_ms = last_timestamp_ms(&tail).or_else(|| last_timestamp_ms(&head));
    if title.is_none() && cwd.is_none() && last_message_ts_ms.is_none() {
        return None;
    }
    Some(SessionEntry {
        session_id: session_id.to_string(),
        adapter: AgentAdapter::ClaudeCode,
        cwd,
        title,
        git_branch,
        last_message_ts_ms,
        size_bytes: Some(len),
        entry_count: None,
    })
}

/// Parse a full Claude `.jsonl` body into a [`SessionEntry`]. Pure over the
/// provided content (exact `entry_count`, `size_bytes` left to the caller);
/// the bounded path above handles oversized logs.
pub fn parse_claude_jsonl(session_id: &str, content: &str) -> Option<SessionEntry> {
    let entry_count = content.lines().filter(|l| line_value(l).is_some()).count();
    if entry_count == 0 {
        return None;
    }
    Some(SessionEntry {
        session_id: session_id.to_string(),
        adapter: AgentAdapter::ClaudeCode,
        cwd: claude_cwd(content),
        title: claude_title(content),
        git_branch: claude_git_branch(content),
        last_message_ts_ms: last_timestamp_ms(content),
        size_bytes: None,
        entry_count: Some(entry_count),
    })
}

/// Row title, matching what Claude's own `/resume` shows: prefer the
/// `lastPrompt` Claude records (already free of command XML), else the first
/// user message with any slash-command wrapper unwrapped.
fn claude_title(content: &str) -> Option<String> {
    claude_last_prompt(content).or_else(|| claude_first_prompt(content))
}

/// `lastPrompt` from a `type=last-prompt` line.
fn claude_last_prompt(content: &str) -> Option<String> {
    for line in content.lines() {
        let Some(v) = line_value(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("last-prompt") {
            continue;
        }
        if let Some(p) = v.get("lastPrompt").and_then(Value::as_str) {
            let cleaned = truncate_prompt(p);
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

/// First non-empty `user` message, slash-command XML unwrapped.
fn claude_first_prompt(content: &str) -> Option<String> {
    for line in content.lines() {
        let Some(v) = line_value(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(text) = user_message_text(&v) {
            let cleaned = clean_command_xml(&text);
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

/// First non-empty `gitBranch` recorded in the log.
fn claude_git_branch(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line_value(line)?
            .get("gitBranch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// Slash-command user messages arrive wrapped as
/// `<command-message>..</command-message><command-name>/x</command-name><command-args>..</command-args>`.
/// Unwrap to a readable `"/x args"`; otherwise strip stray tags + collapse.
fn clean_command_xml(s: &str) -> String {
    if let Some(name) = tag_inner(s, "command-name") {
        let args = tag_inner(s, "command-args").unwrap_or_default();
        return truncate_prompt(&format!("{name} {args}"));
    }
    truncate_prompt(&strip_tags(s))
}

/// Inner text of the first `<tag>…</tag>`, trimmed.
fn tag_inner(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_string())
}

/// Drop `<…>` tag runs, keep the text between them.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// `message.content` as text: a bare string, or the first `text` block of an
/// array of content blocks.
fn user_message_text(v: &Value) -> Option<String> {
    let content = v.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    arr.iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .find_map(|b| b.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

/// First recorded `cwd` field — present on most Claude log entries.
fn claude_cwd(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line_value(line)?
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// Last parseable `timestamp` in the body (chronological logs ⇒ newest).
fn last_timestamp_ms(content: &str) -> Option<i64> {
    let mut last = None;
    for line in content.lines() {
        let Some(v) = line_value(line) else { continue };
        if let Some(ms) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms)
        {
            last = Some(ms);
        }
    }
    last
}

// --- Codex -----------------------------------------------------------------

fn collect_codex(codex_dir: &Path, out: &mut Vec<SessionEntry>) {
    let index = codex_dir.join("session_index.jsonl");
    let Ok(content) = fs::read_to_string(&index) else {
        return;
    };
    for line in content.lines() {
        if let Some(entry) = parse_codex_index_line(line) {
            out.push(entry);
        }
    }
}

/// Parse one `{id, thread_name, updated_at}` line of the Codex session index.
pub fn parse_codex_index_line(line: &str) -> Option<SessionEntry> {
    let v = line_value(line)?;
    let id = v.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let title = v
        .get("thread_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(truncate_prompt);
    let last_message_ts_ms = v.get("updated_at").and_then(parse_flexible_ts);
    Some(SessionEntry {
        session_id: id.to_string(),
        adapter: AgentAdapter::Codex,
        cwd: None,
        title,
        git_branch: None,
        last_message_ts_ms,
        size_bytes: None,
        entry_count: None,
    })
}

/// Codex `updated_at` shape is unconfirmed against real data, so accept the
/// plausible forms: RFC-3339 string, integer-in-string, or numeric epoch
/// (seconds or millis). Anything else → `None` (never panics).
fn parse_flexible_ts(v: &Value) -> Option<i64> {
    if let Some(s) = v.as_str() {
        return parse_timestamp_ms(s).or_else(|| s.parse::<i64>().ok().map(normalize_epoch));
    }
    if let Some(n) = v.as_i64() {
        return Some(normalize_epoch(n));
    }
    v.as_f64().map(|f| normalize_epoch(f as i64))
}

/// Epoch seconds (~1.7e9 today) vs millis (~1.7e12): scale up to millis when
/// the magnitude is clearly seconds.
fn normalize_epoch(n: i64) -> i64 {
    if n.abs() < 100_000_000_000 {
        n * 1000
    } else {
        n
    }
}

// --- shared helpers --------------------------------------------------------

fn line_value(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line).ok()
}

/// One-line, whitespace-collapsed, length-capped prompt preview.
fn truncate_prompt(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PROMPT_MAX_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(PROMPT_MAX_CHARS).collect();
    out.push('…');
    out
}

fn read_head(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let f = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_user_line(ts: &str, cwd: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","cwd":"{cwd}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn claude_assistant_line(ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
    }

    #[test]
    fn session_index_parses_jsonl_fixture() {
        let content = format!(
            "{}\n{}\n",
            claude_user_line("2026-06-20T10:00:00Z", "/Users/x/proj", "refactor the parser"),
            claude_assistant_line("2026-06-20T10:05:00Z"),
        );
        let e = parse_claude_jsonl("sess-uuid", &content).unwrap();
        assert_eq!(e.session_id, "sess-uuid");
        assert_eq!(e.adapter, AgentAdapter::ClaudeCode);
        assert_eq!(e.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(e.title.as_deref(), Some("refactor the parser"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T10:05:00Z"));
        assert_eq!(e.entry_count, Some(2));
    }

    #[test]
    fn claude_first_prompt_reads_array_content_block() {
        let line = r#"{"type":"user","timestamp":"2026-06-20T10:00:00Z","message":{"content":[{"type":"text","text":"hello there"}]}}"#;
        let e = parse_claude_jsonl("s", line).unwrap();
        assert_eq!(e.title.as_deref(), Some("hello there"));
    }

    #[test]
    fn claude_first_prompt_collapses_whitespace() {
        let line = claude_user_line("2026-06-20T10:00:00Z", "/p", "line one\\nline   two");
        let e = parse_claude_jsonl("s", &line).unwrap();
        // The literal "\n" in JSON decodes to a newline; preview flattens it.
        assert_eq!(e.title.as_deref(), Some("line one line two"));
    }

    #[test]
    fn title_prefers_last_prompt_over_first_user_message() {
        let last_prompt = r#"{"type":"last-prompt","lastPrompt":"/cook plan.md","sessionId":"s"}"#;
        let content = format!(
            "{last_prompt}\n{}\n",
            claude_user_line("2026-06-20T10:00:00Z", "/p", "the first message"),
        );
        let e = parse_claude_jsonl("s", &content).unwrap();
        assert_eq!(e.title.as_deref(), Some("/cook plan.md"));
    }

    #[test]
    fn title_unwraps_slash_command_xml() {
        // A slash-command user message with no last-prompt line falls back to
        // the unwrapped first message: "/cook <args>", not the raw tags.
        let xml = "<command-message>cook</command-message><command-name>/cook</command-name><command-args>plan.md here</command-args>";
        let line = claude_user_line("2026-06-20T10:00:00Z", "/p", xml);
        let e = parse_claude_jsonl("s", &line).unwrap();
        assert_eq!(e.title.as_deref(), Some("/cook plan.md here"));
    }

    #[test]
    fn parses_git_branch_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude").join("projects").join("-p");
        std::fs::create_dir_all(&proj).unwrap();
        let line = format!(
            r#"{{"type":"user","timestamp":"2026-06-20T10:00:00Z","cwd":"/p","gitBranch":"feat/x","message":{{"content":"hi"}}}}"#
        );
        let path = proj.join("s.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let entries = SessionIndex::build(&tmp.path().join(".claude"), Path::new("/nonexistent"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].git_branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[0].size_bytes, Some(std::fs::metadata(&path).unwrap().len()));
    }

    #[test]
    fn parse_codex_index_line_reads_id_thread_updated() {
        let line = r#"{"id":"abc-123","thread_name":"fix flaky test","updated_at":"2026-06-19T09:00:00Z"}"#;
        let e = parse_codex_index_line(line).unwrap();
        assert_eq!(e.session_id, "abc-123");
        assert_eq!(e.adapter, AgentAdapter::Codex);
        assert_eq!(e.cwd, None);
        assert_eq!(e.title.as_deref(), Some("fix flaky test"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-19T09:00:00Z"));
    }

    #[test]
    fn parse_codex_index_line_accepts_epoch_seconds() {
        let line = r#"{"id":"x","thread_name":"t","updated_at":1718784000}"#;
        let e = parse_codex_index_line(line).unwrap();
        assert_eq!(e.last_message_ts_ms, Some(1_718_784_000_000));
    }

    #[test]
    fn malformed_lines_are_skipped_without_panic() {
        assert!(parse_claude_jsonl("s", "not json\n{bad").is_none());
        assert!(parse_codex_index_line("garbage").is_none());
        assert!(parse_codex_index_line(r#"{"thread_name":"no id"}"#).is_none());
        assert!(parse_codex_index_line(r#"{"id":""}"#).is_none());
    }

    #[test]
    fn build_scans_claude_and_codex_sorted_desc() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let proj = claude.join("projects").join("-Users-x-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&codex).unwrap();

        std::fs::write(
            proj.join("old.jsonl"),
            format!("{}\n", claude_user_line("2026-06-18T08:00:00Z", "/Users/x/proj", "old task")),
        )
        .unwrap();
        std::fs::write(
            proj.join("new.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj", "new task")),
        )
        .unwrap();
        std::fs::write(
            codex.join("session_index.jsonl"),
            "{\"id\":\"cx\",\"thread_name\":\"codex task\",\"updated_at\":\"2026-06-19T08:00:00Z\"}\n",
        )
        .unwrap();

        let entries = SessionIndex::build(&claude, &codex);
        assert_eq!(entries.len(), 3);
        // Newest first: new (06-20) > codex (06-19) > old (06-18).
        assert_eq!(entries[0].session_id, "new");
        assert_eq!(entries[0].title.as_deref(), Some("new task"));
        assert_eq!(entries[1].session_id, "cx");
        assert_eq!(entries[1].adapter, AgentAdapter::Codex);
        assert_eq!(entries[2].session_id, "old");
    }

    #[test]
    fn build_missing_dirs_is_empty() {
        let entries =
            SessionIndex::build(Path::new("/nonexistent/claude"), Path::new("/nonexistent/codex"));
        assert!(entries.is_empty());
    }

    #[test]
    fn large_claude_log_uses_bounded_path_with_no_entry_count() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude").join("projects").join("-p");
        std::fs::create_dir_all(&proj).unwrap();

        let mut content =
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/p", "kick off"));
        let filler = claude_assistant_line("2026-06-20T08:01:00Z");
        while (content.len() as u64) <= FULL_PARSE_LIMIT + 4096 {
            content.push_str(&filler);
            content.push('\n');
        }
        // A recognizable newest timestamp at the very end (within the tail).
        content.push_str(&format!("{}\n", claude_assistant_line("2026-06-20T09:00:00Z")));
        std::fs::write(proj.join("big.jsonl"), &content).unwrap();

        let entries = SessionIndex::build(&tmp.path().join(".claude"), Path::new("/nonexistent"));
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.session_id, "big");
        assert_eq!(e.title.as_deref(), Some("kick off"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T09:00:00Z"));
        assert_eq!(e.entry_count, None);
    }
}
