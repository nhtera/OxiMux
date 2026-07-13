//! Read-only index of past agent sessions for a history/resume picker.
//!
//! Two on-disk sources, both journaled by the agent CLIs themselves:
//!
//! - Claude Code — one `.jsonl` per session under
//!   `<claude_dir>/projects/<cwd-slug>/<session-uuid>.jsonl` (the same files
//!   [`super::activity`] tails). The file stem IS the session id; the first
//!   `user` entry is the opening prompt; entries carry `cwd` + `timestamp`.
//! - Codex — one rollout `.jsonl` per session under
//!   `<codex_dir>/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (the store
//!   `codex resume` lists and [`crate::thread::codex_session_import`] reopens).
//!   The head's `session_meta` line carries the id, cwd, git branch, and start
//!   time; the first non-injected `user` message is the opening prompt.
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
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

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
        // Codex rollouts record their cwd, so they scope by project just like
        // Claude's — no all-projects-only special case.
        collect_codex(codex_dir, scope, &mut entries);
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
/// the preview pane which applies its own longer limit. Command parsing is the
/// shared [`crate::command_envelope`] helper; the non-command fallback keeps the
/// lossy stray-tag strip (fine for a one-line preview, unlike a full transcript).
pub(super) fn unwrap_command_xml(s: &str) -> String {
    if let Some(cmd) = crate::command_envelope::parse_slash_command(s) {
        return cmd.normalized();
    }
    strip_tags(s)
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

/// Walk `<codex_dir>/sessions/**/rollout-*.jsonl` and build one entry per
/// rollout. A scoped build keeps only rollouts whose recorded `cwd` matches an
/// active project path (Codex records cwd in `session_meta`, so it scopes just
/// like Claude). An unreadable tree contributes nothing.
fn collect_codex(codex_dir: &Path, scope: &SessionScope, out: &mut Vec<SessionEntry>) {
    let sessions = codex_dir.join("sessions");
    let mut files = Vec::new();
    collect_rollout_files(&sessions, 0, &mut files);
    for path in files {
        let Some(entry) = build_codex_entry(&path) else {
            continue;
        };
        if let SessionScope::Projects(targets) = scope {
            let keep = entry
                .cwd
                .as_deref()
                .is_some_and(|c| targets.iter().any(|t| codex_cwd_matches(c, t)));
            if !keep {
                continue;
            }
        }
        out.push(entry);
    }
}

/// Depth-bounded collection of every `rollout-*.jsonl` under the sessions tree.
/// Codex nests three levels deep (`YYYY/MM/DD`); the cap guards a pathological
/// tree from unbounded recursion. Symlinked entries are never followed.
fn collect_rollout_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 5;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_rollout_files(&path, depth + 1, out);
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with("rollout-")
            && name.ends_with(".jsonl")
        {
            out.push(path);
        }
    }
}

/// Build one [`SessionEntry`] from a Codex rollout. Reads only the head (up to
/// the first genuine prompt) plus a bounded tail (for the newest timestamp).
/// Returns `None` when no `session_meta` id is found (a torn or foreign file).
fn build_codex_entry(path: &Path) -> Option<SessionEntry> {
    let head = parse_codex_head(path)?;
    let tail = read_tail(path, TAIL_BYTES).unwrap_or_default();
    let last_message_ts_ms = last_timestamp_ms(&tail).or(head.created_at_ms);
    let size_bytes = fs::metadata(path).ok().map(|m| m.len());
    Some(SessionEntry {
        session_id: head.session_id,
        adapter: AgentAdapter::Codex,
        path: Some(path.to_string_lossy().into_owned()),
        cwd: head.cwd,
        title: head.title,
        custom_title: None,
        git_branch: head.git_branch,
        tag: None,
        created_at_ms: head.created_at_ms,
        last_message_ts_ms,
        message_count: None,
        size_bytes,
        entry_count: None,
    })
}

/// The fields a rollout head yields: the `session_meta` identity plus the first
/// genuine user prompt (the title).
struct CodexHead {
    session_id: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    created_at_ms: Option<i64>,
    title: Option<String>,
}

/// Stream a rollout's leading lines to extract [`CodexHead`]. Codex inlines a
/// large `AGENTS.md` block as the first synthetic user turn, so the real prompt
/// can sit tens of KB in — past any fixed head window. Scanning stops as soon as
/// the prompt is found, so a typical read is a few KB; a preamble-only or torn
/// file is bounded by [`MAX_SCAN_BYTES`].
fn parse_codex_head(path: &Path) -> Option<CodexHead> {
    /// Stop scanning after this many bytes even without a genuine prompt, so a
    /// title-less rollout never reads unbounded. Covers the observed worst-case
    /// prompt offset with headroom.
    const MAX_SCAN_BYTES: u64 = 512 * 1024;
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut scanned: u64 = 0;
    let mut session_id: Option<String> = None;
    let mut cwd = None;
    let mut git_branch = None;
    let mut created_at_ms = None;
    let mut title: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        scanned += n as u64;
        if let Some(v) = line_value(line.trim_end()) {
            match v.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(p) = v.get("payload") {
                        // Newer rollouts key the id as `session_id`; older ones
                        // (pre-0.14 CLI) use `id`. Both embed it in the filename.
                        session_id = p
                            .get("session_id")
                            .or_else(|| p.get("id"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        cwd = p
                            .get("cwd")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        git_branch = p
                            .pointer("/git/branch")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        created_at_ms = p
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms);
                    }
                }
                // A turn: newer rollouts wrap it as `response_item.payload`;
                // older ones emit a top-level `type: "message"`.
                Some("response_item") if title.is_none() => {
                    title = v.get("payload").and_then(codex_user_prompt);
                }
                Some("message") if title.is_none() => {
                    title = codex_user_prompt(&v);
                }
                _ => {}
            }
        }
        if title.is_some() || scanned >= MAX_SCAN_BYTES {
            break;
        }
    }
    Some(CodexHead {
        session_id: session_id?,
        cwd,
        git_branch,
        created_at_ms,
        title,
    })
}

/// The genuine first user prompt from a rollout message object (a
/// `response_item.payload` or an older top-level `message`), or `None` when it
/// is not a user message or is injected context. Codex prepends the project
/// `AGENTS.md` and `<…>`-wrapped context blocks (`<environment_context>`,
/// `<user_instructions>`, `<recommended_plugins>`, …) as synthetic user turns;
/// the title should be the human's actual first line, matching `codex resume`.
fn codex_user_prompt(msg: &Value) -> Option<String> {
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    // Guard the message type when present (newer payloads carry it; older
    // top-level message lines don't, and their `type` is the outer `message`).
    if let Some(t) = msg.get("type").and_then(Value::as_str)
        && t != "message"
    {
        return None;
    }
    let text = msg
        .get("content")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("input_text"))
        .find_map(|b| b.get("text").and_then(Value::as_str))?;
    if is_codex_injected_prompt(text) {
        return None;
    }
    let cleaned = truncate_prompt(text);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Is this synthetic-user text an injected context block rather than a real
/// prompt? Injected blocks are `<…>`-tag-wrapped or the `AGENTS.md` instructions.
fn is_codex_injected_prompt(text: &str) -> bool {
    let s = text.trim_start();
    s.starts_with('<') || s.starts_with("# AGENTS.md")
}

/// Match a rollout's recorded `cwd` against an active project path. Exact string
/// match first; then a canonicalized comparison so symlink-equivalent paths
/// (e.g. `/tmp` ↔ `/private/tmp` on macOS) still scope together.
fn codex_cwd_matches(cwd: &str, target: &str) -> bool {
    if cwd == target {
        return true;
    }
    matches!(
        (fs::canonicalize(cwd), fs::canonicalize(target)),
        (Ok(a), Ok(b)) if a == b
    )
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
