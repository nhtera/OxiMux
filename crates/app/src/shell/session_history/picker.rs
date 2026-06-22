//! Pure presentation + launch logic for the session-history picker.
//!
//! All the parts that don't touch GPUI live here so they're unit-testable:
//! the one-line row label + dim detail line, the fuzzy filter over those
//! labels (reusing the command-palette matcher), the resume-vs-fork →
//! [`SessionResumption`] mapping, and the fork-with-context preamble. The
//! modal view in `mod.rs` stays a thin shell over these.

use oximux_agents::session_log::session_index::SessionEntry;
use oximux_core::{AgentAdapter, SessionResumption};

use crate::shell::command_palette::match_engine::filter_and_rank;

/// Which way to relaunch the highlighted session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    /// Continue the same session in place (`--resume` / `resume`).
    Resume,
    /// Branch a new session off it (`--fork-session` / `fork`).
    Fork,
}

impl LaunchKind {
    /// Map the chosen action onto the adapter-agnostic resumption value the
    /// spawn layer consumes.
    pub fn resumption(self, id: String) -> SessionResumption {
        match self {
            LaunchKind::Resume => SessionResumption::Resume { id },
            LaunchKind::Fork => SessionResumption::Fork { id },
        }
    }
}

/// Static registry slug `spawn_agent_tab` expects for a built-in adapter.
pub fn adapter_slug(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

/// Short lowercase tag shown at the head of a row.
fn adapter_tag(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

/// Searchable + displayed title line — the session's clean prompt/summary
/// (no adapter prefix, mirroring Claude's `/resume`).
pub fn session_row_title(entry: &SessionEntry) -> String {
    entry
        .title
        .clone()
        .unwrap_or_else(|| "(untitled session)".to_string())
}

/// Dim subtitle, mirroring Claude's `/resume`: `relative-time · branch · size`.
/// Codex rows carry no branch/size, so they lead with the adapter name. When
/// `show_path` is set (the all-projects view) the session's `cwd` is appended
/// — `~`-abbreviated against `home` — so the project each row belongs to is
/// visible; the scoped view omits it (every row shares the repo). `now_ms` and
/// `home` are passed in so the function stays pure (testable).
pub fn session_row_subtitle(
    entry: &SessionEntry,
    now_ms: i64,
    show_path: bool,
    home: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if entry.adapter != AgentAdapter::ClaudeCode {
        parts.push(adapter_tag(entry.adapter).to_string());
    }
    if let Some(ts) = entry.last_message_ts_ms {
        parts.push(relative_age(now_ms.saturating_sub(ts)));
    }
    if let Some(branch) = entry.git_branch.as_deref().filter(|b| !b.is_empty()) {
        parts.push(branch.to_string());
    }
    if let Some(size) = entry.size_bytes {
        parts.push(format_size(size));
    }
    if show_path
        && let Some(cwd) = entry.cwd.as_deref().filter(|c| !c.is_empty())
    {
        parts.push(home_abbrev(cwd, home));
    }
    parts.join(" · ")
}

/// Replace a leading `home` directory with `~` (`/Users/x/Code` → `~/Code`).
/// Leaves the path untouched when it isn't under `home`.
fn home_abbrev(path: &str, home: Option<&str>) -> String {
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home)
            && rest.starts_with('/')
        {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Filter + rank entries by `query` over their titles. Returns indices into
/// `entries`, newest-first when the query is empty (entries arrive sorted).
pub fn filter_sessions(query: &str, entries: &[SessionEntry]) -> Vec<usize> {
    let titles: Vec<String> = entries.iter().map(session_row_title).collect();
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    filter_and_rank(query, &refs)
}

/// Preamble prepended to stripped scrollback when forking with context.
pub fn fork_context_preamble(id: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(c) => format!("Context from session {id} ({c}):\n"),
        None => format!("Context from session {id}:\n"),
    }
}

/// Verbose relative age matching Claude's resume picker: "9 seconds ago",
/// "6 minutes ago", "1 day ago". Clamps negatives (clock skew) to "just now".
fn relative_age(delta_ms: i64) -> String {
    if delta_ms <= 0 {
        return "just now".to_string();
    }
    let secs = delta_ms / 1000;
    if secs < 60 {
        return ago(secs.max(1), "second");
    }
    let mins = secs / 60;
    if mins < 60 {
        return ago(mins, "minute");
    }
    let hours = mins / 60;
    if hours < 24 {
        return ago(hours, "hour");
    }
    ago(hours / 24, "day")
}

fn ago(n: i64, unit: &str) -> String {
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Human-readable byte size, matching Claude: "21.3MB", "70.1KB", "512B".
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fields<'a> {
        adapter: AgentAdapter,
        title: Option<&'a str>,
        branch: Option<&'a str>,
        ts: Option<i64>,
        size: Option<u64>,
    }

    fn entry(f: Fields<'_>) -> SessionEntry {
        SessionEntry {
            session_id: "id".into(),
            adapter: f.adapter,
            path: None,
            cwd: None,
            title: f.title.map(str::to_string),
            git_branch: f.branch.map(str::to_string),
            last_message_ts_ms: f.ts,
            size_bytes: f.size,
            entry_count: None,
        }
    }

    fn claude(title: Option<&str>) -> SessionEntry {
        entry(Fields {
            adapter: AgentAdapter::ClaudeCode,
            title,
            branch: None,
            ts: None,
            size: None,
        })
    }

    #[test]
    fn launch_kind_maps_to_resumption() {
        assert_eq!(
            LaunchKind::Resume.resumption("a".into()),
            SessionResumption::Resume { id: "a".into() }
        );
        assert_eq!(
            LaunchKind::Fork.resumption("b".into()),
            SessionResumption::Fork { id: "b".into() }
        );
    }

    #[test]
    fn adapter_slug_matches_spawn_registry_ids() {
        assert_eq!(adapter_slug(AgentAdapter::ClaudeCode), "claude-code");
        assert_eq!(adapter_slug(AgentAdapter::Codex), "codex");
        assert_eq!(adapter_slug(AgentAdapter::Aider), "aider");
        assert_eq!(adapter_slug(AgentAdapter::Custom), "custom");
    }

    #[test]
    fn row_title_is_the_clean_prompt_no_prefix() {
        let e = claude(Some("refactor the parser"));
        assert_eq!(session_row_title(&e), "refactor the parser");
    }

    #[test]
    fn row_title_falls_back_when_absent() {
        let e = claude(None);
        assert_eq!(session_row_title(&e), "(untitled session)");
    }

    #[test]
    fn subtitle_is_time_branch_size_for_claude() {
        let now = 10_000_000_000;
        let e = entry(Fields {
            adapter: AgentAdapter::ClaudeCode,
            title: Some("p"),
            branch: Some("main"),
            ts: Some(now - 3 * 60 * 60 * 1000), // 3 hours ago
            size: Some(70_100),
        });
        // Scoped view (show_path=false): no path.
        assert_eq!(
            session_row_subtitle(&e, now, false, None),
            "3 hours ago · main · 68.5KB"
        );
    }

    #[test]
    fn subtitle_appends_abbreviated_path_in_all_mode() {
        let now = 10_000_000_000;
        let mut e = entry(Fields {
            adapter: AgentAdapter::ClaudeCode,
            title: Some("p"),
            branch: Some("main"),
            ts: Some(now - 60 * 1000),
            size: Some(1024),
        });
        e.cwd = Some("/Users/x/Code/proj".to_string());
        // All-projects view (show_path=true): cwd appended, ~-abbreviated.
        assert_eq!(
            session_row_subtitle(&e, now, true, Some("/Users/x")),
            "1 minute ago · main · 1.0KB · ~/Code/proj"
        );
        // Path outside home stays absolute.
        assert_eq!(
            session_row_subtitle(&e, now, true, Some("/other")),
            "1 minute ago · main · 1.0KB · /Users/x/Code/proj"
        );
    }

    #[test]
    fn home_abbrev_handles_exact_and_non_home() {
        assert_eq!(home_abbrev("/Users/x", Some("/Users/x")), "~");
        assert_eq!(home_abbrev("/Users/x/a", Some("/Users/x")), "~/a");
        // Not a path-boundary match — don't abbreviate a sibling prefix.
        assert_eq!(home_abbrev("/Users/xyz/a", Some("/Users/x")), "/Users/xyz/a");
        assert_eq!(home_abbrev("/tmp/a", None), "/tmp/a");
    }

    #[test]
    fn subtitle_leads_with_adapter_for_non_claude() {
        let now = 10_000_000_000;
        let e = entry(Fields {
            adapter: AgentAdapter::Codex,
            title: Some("t"),
            branch: None,
            ts: Some(now - 60 * 1000), // 1 minute ago
            size: None,
        });
        assert_eq!(
            session_row_subtitle(&e, now, false, None),
            "codex · 1 minute ago"
        );
    }

    #[test]
    fn filter_sessions_ranks_by_title_and_excludes_non_matches() {
        let entries = vec![claude(Some("write tests")), claude(Some("refactor parser"))];
        // "refactor" matches only the second entry.
        assert_eq!(filter_sessions("refactor", &entries), vec![1]);
        // Empty query returns all in original (newest-first) order.
        assert_eq!(filter_sessions("", &entries), vec![0, 1]);
        // No match → empty.
        assert!(filter_sessions("zzzzz", &entries).is_empty());
    }

    #[test]
    fn fork_preamble_includes_cwd_when_present() {
        assert_eq!(
            fork_context_preamble("abc", Some("/tmp/proj")),
            "Context from session abc (/tmp/proj):\n"
        );
        assert_eq!(
            fork_context_preamble("abc", None),
            "Context from session abc:\n"
        );
    }

    #[test]
    fn relative_age_is_verbose_and_pluralized() {
        assert_eq!(relative_age(-5), "just now");
        assert_eq!(relative_age(9 * 1000), "9 seconds ago");
        assert_eq!(relative_age(60 * 1000), "1 minute ago");
        assert_eq!(relative_age(6 * 60 * 1000), "6 minutes ago");
        assert_eq!(relative_age(60 * 60 * 1000), "1 hour ago");
        assert_eq!(relative_age(2 * 24 * 60 * 60 * 1000), "2 days ago");
    }

    #[test]
    fn format_size_matches_claude_units() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(70_100), "68.5KB");
        assert_eq!(format_size(21_300_000), "20.3MB");
    }
}
