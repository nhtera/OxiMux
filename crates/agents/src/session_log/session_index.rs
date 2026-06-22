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
    /// On-disk path to the session log, for the preview pane. `Some` for
    /// Claude (the `.jsonl`); `None` for Codex (the compact index has no
    /// per-session file path).
    pub path: Option<String>,
    /// Launch directory, when the log records one (Codex index omits it).
    pub cwd: Option<String>,
    /// Row title: the user/AI-assigned name (`customTitle`/`aiTitle`) if any,
    /// else Claude's `lastPrompt`, else the first user message with
    /// slash-command XML unwrapped (Codex: thread name). One line, capped.
    pub title: Option<String>,
    /// The user-rename / AI-generated title alone (`customTitle` ▸ `aiTitle`),
    /// kept distinct from `title` so a future rename knows one already exists.
    pub custom_title: Option<String>,
    /// Git branch the session ran on (Claude `gitBranch`; absent for Codex).
    pub git_branch: Option<String>,
    /// User-assigned tag for the session, when present (Claude `{"type":"tag"}`).
    pub tag: Option<String>,
    /// First entry timestamp in unix millis — when the session started.
    pub created_at_ms: Option<i64>,
    /// Newest entry timestamp in unix millis; the listing sort key.
    pub last_message_ts_ms: Option<i64>,
    /// User + assistant turn count — exact when the whole log was parsed,
    /// `None` when only the head/tail of an oversized log was read.
    pub message_count: Option<usize>,
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

/// Which projects the index should cover.
///
/// Mirrors Claude Code's `/resume`: by default it lists only the sessions of
/// the current repo (the launch dir plus its git worktrees), and `Ctrl+A`
/// flips to every project on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScope {
    /// Every project under `<claude_dir>/projects` — the "show all" view.
    AllProjects,
    /// Only sessions whose Claude project directory matches one of these
    /// launch paths (the active project root + its git worktrees). An empty
    /// list still scopes (matches nothing) — callers fall back to
    /// [`SessionScope::AllProjects`] when there's no active project.
    Projects(Vec<String>),
}

/// Longest single path component most filesystems allow before Claude Code
/// truncates the slug and appends a hash (its `MAX_SANITIZED_LENGTH`).
const MAX_SANITIZED_LEN: usize = 200;

/// Map a launch directory to its `<claude_dir>/projects` slug exactly as the
/// agent CLI does: every non-alphanumeric byte becomes `-`
/// (`/Users/x/My App` → `-Users-x-My-App`).
pub fn sanitize_project_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Does an on-disk project-dir name correspond to a sanitized launch path?
/// Exact match for normal paths; over-long paths get truncated + a hash we
/// don't reproduce, so fall back to a prefix match for those.
fn slug_matches(dir_name: &str, sanitized_target: &str) -> bool {
    if dir_name == sanitized_target {
        return true;
    }
    sanitized_target.len() > MAX_SANITIZED_LEN
        && dir_name.starts_with(&sanitized_target[..MAX_SANITIZED_LEN])
}

/// Build the merged, newest-first session index from the two CLI state
/// roots (normally `~/.claude` and `~/.codex`). Missing roots contribute
/// nothing.
pub struct SessionIndex;

impl SessionIndex {
    pub fn build(claude_dir: &Path, codex_dir: &Path, scope: &SessionScope) -> Vec<SessionEntry> {
        let mut entries = Vec::new();
        collect_claude(claude_dir, scope, &mut entries);
        // Codex's compact index records no cwd, so its sessions can't be tied
        // to a project — only surface them in the unscoped (all) view.
        if matches!(scope, SessionScope::AllProjects) {
            collect_codex(codex_dir, &mut entries);
        }
        // Newest first; entries without a timestamp sort to the bottom.
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_message_ts_ms));
        entries
    }
}

// --- Claude Code -----------------------------------------------------------

fn collect_claude(claude_dir: &Path, scope: &SessionScope, out: &mut Vec<SessionEntry>) {
    let projects = claude_dir.join("projects");
    let Ok(project_dirs) = fs::read_dir(&projects) else {
        return;
    };
    // In scoped mode, precompute the sanitized project-dir names we accept.
    let targets: Option<Vec<String>> = match scope {
        SessionScope::AllProjects => None,
        SessionScope::Projects(paths) => {
            Some(paths.iter().map(|p| sanitize_project_path(p)).collect())
        }
    };
    for proj in project_dirs.flatten() {
        // Skip symlinked project dirs — never follow links out of the root.
        if !proj.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
            continue;
        }
        // Scoped: keep only project dirs whose slug matches a target path.
        if let Some(targets) = &targets {
            let name = proj.file_name();
            let name = name.to_string_lossy();
            if !targets.iter().any(|t| slug_matches(&name, t)) {
                continue;
            }
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
        entry.path = Some(path.to_string_lossy().into_owned());
        return Some(entry);
    }
    // Large log: the opening prompt, cwd, branch, and start time live in the
    // head; titles/tags appended late (customTitle/aiTitle/tag) live in the
    // tail; the newest timestamp is at the very end.
    let head = read_head(path, HEAD_BYTES).unwrap_or_default();
    let tail = read_tail(path, TAIL_BYTES).unwrap_or_default();
    let custom_title = claude_custom_title(&tail).or_else(|| claude_custom_title(&head));
    let title = custom_title
        .clone()
        .or_else(|| claude_last_prompt(&head))
        .or_else(|| claude_first_prompt(&head))
        .or_else(|| claude_title(&tail));
    let cwd = claude_cwd(&head).or_else(|| claude_cwd(&tail));
    let git_branch = claude_git_branch(&head).or_else(|| claude_git_branch(&tail));
    let tag = claude_tag(&tail).or_else(|| claude_tag(&head));
    let created_at_ms = first_timestamp_ms(&head);
    let last_message_ts_ms = last_timestamp_ms(&tail).or_else(|| last_timestamp_ms(&head));
    if title.is_none() && cwd.is_none() && last_message_ts_ms.is_none() {
        return None;
    }
    Some(SessionEntry {
        session_id: session_id.to_string(),
        adapter: AgentAdapter::ClaudeCode,
        path: Some(path.to_string_lossy().into_owned()),
        cwd,
        title,
        custom_title,
        git_branch,
        tag,
        created_at_ms,
        last_message_ts_ms,
        // A partial head/tail read can't yield an exact turn count.
        message_count: None,
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
        // Filled by `build_claude_entry` (this fn is pure over content).
        path: None,
        cwd: claude_cwd(content),
        title: claude_title(content),
        custom_title: claude_custom_title(content),
        git_branch: claude_git_branch(content),
        tag: claude_tag(content),
        created_at_ms: first_timestamp_ms(content),
        last_message_ts_ms: last_timestamp_ms(content),
        message_count: Some(claude_message_count(content)),
        size_bytes: None,
        entry_count: Some(entry_count),
    })
}

/// Row title, matching what Claude's own `/resume` shows: prefer a
/// user/AI-assigned title (`customTitle`/`aiTitle`), else the `lastPrompt`
/// Claude records (already free of command XML), else the first user message
/// with any slash-command wrapper unwrapped.
fn claude_title(content: &str) -> Option<String> {
    claude_custom_title(content)
        .or_else(|| claude_last_prompt(content))
        .or_else(|| claude_first_prompt(content))
}

/// The session's assigned name: the last `customTitle` (a user rename) if any,
/// else the last `aiTitle` (auto-generated summary). Both are appended late in
/// the log, so a tail read finds them. Capped like any title.
fn claude_custom_title(content: &str) -> Option<String> {
    let mut custom = None;
    let mut ai = None;
    for line in content.lines() {
        let Some(v) = line_value(line) else { continue };
        if let Some(t) = v
            .get("customTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            custom = Some(t.to_string());
        }
        if let Some(t) = v
            .get("aiTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            ai = Some(t.to_string());
        }
    }
    custom.or(ai).map(|t| truncate_prompt(&t))
}

/// The session's tag, from a dedicated `{"type":"tag"}` line (last wins). The
/// type guard avoids matching a `tag` parameter inside a tool-call input.
fn claude_tag(content: &str) -> Option<String> {
    let mut tag = None;
    for line in content.lines() {
        let Some(v) = line_value(line) else { continue };
        if v.get("type").and_then(Value::as_str) == Some("tag")
            && let Some(t) = v.get("tag").and_then(Value::as_str).filter(|s| !s.is_empty())
        {
            tag = Some(t.to_string());
        }
    }
    tag
}

/// First parseable `timestamp` in the body — when the session started.
fn first_timestamp_ms(content: &str) -> Option<i64> {
    content.lines().find_map(|line| {
        line_value(line)?
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms)
    })
}

/// Count of `user` + `assistant` turns (skips meta/tool_result user echoes).
fn claude_message_count(content: &str) -> usize {
    content
        .lines()
        .filter_map(line_value)
        .filter(|v| match v.get("type").and_then(Value::as_str) {
            Some("assistant") => true,
            Some("user") => {
                v.get("isMeta").and_then(Value::as_bool) != Some(true)
                    && v.pointer("/message/content")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            !arr.iter().any(|b| {
                                b.get("type").and_then(Value::as_str) == Some("tool_result")
                            })
                        })
                        .unwrap_or(true)
            }
            _ => false,
        })
        .count()
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
/// Title use caps the result; preview use ([`unwrap_command_xml`]) does not.
fn clean_command_xml(s: &str) -> String {
    truncate_prompt(&unwrap_command_xml(s))
}

/// The slash-command unwrap (and stray-tag strip) without the title cap, for
/// the preview pane which applies its own longer limit.
pub(super) fn unwrap_command_xml(s: &str) -> String {
    if let Some(name) = tag_inner(s, "command-name") {
        let args = tag_inner(s, "command-args").unwrap_or_default();
        return format!("{name} {args}").trim().to_string();
    }
    strip_tags(s)
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
pub(super) fn user_message_text(v: &Value) -> Option<String> {
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
        path: None,
        cwd: None,
        title,
        custom_title: None,
        git_branch: None,
        tag: None,
        created_at_ms: None,
        last_message_ts_ms,
        message_count: None,
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

pub(super) fn line_value(line: &str) -> Option<Value> {
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

pub(super) fn read_head(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let f = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
#[path = "session_index_tests.rs"]
mod tests;
