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

/// Searchable + displayed primary line: `"claude · refactor the parser"`.
pub fn session_row_label(entry: &SessionEntry) -> String {
    let prompt = entry.first_prompt.as_deref().unwrap_or("(no prompt)");
    format!("{} · {}", adapter_tag(entry.adapter), prompt)
}

/// Dim secondary line: shortened cwd + relative age, whichever are known.
/// `now_ms` is passed in so the function stays pure (testable).
pub fn session_row_detail(entry: &SessionEntry, now_ms: i64) -> String {
    let cwd = entry.cwd.as_deref().map(short_path).unwrap_or_default();
    match entry.last_message_ts_ms {
        Some(ts) => {
            let age = relative_age(now_ms.saturating_sub(ts));
            if cwd.is_empty() {
                age
            } else {
                format!("{cwd} · {age}")
            }
        }
        None => cwd,
    }
}

/// Filter + rank entries by `query` over their labels. Returns indices into
/// `entries`, newest-first when the query is empty (entries arrive sorted).
pub fn filter_sessions(query: &str, entries: &[SessionEntry]) -> Vec<usize> {
    let labels: Vec<String> = entries.iter().map(session_row_label).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    filter_and_rank(query, &refs)
}

/// Preamble prepended to stripped scrollback when forking with context.
pub fn fork_context_preamble(id: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(c) => format!("Context from session {id} ({c}):\n"),
        None => format!("Context from session {id}:\n"),
    }
}

/// Last two path segments (or the whole thing if shorter): `oximux/crates`.
fn short_path(path: &str) -> String {
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let n = segments.len();
    if n <= 2 {
        segments.join("/")
    } else {
        format!("{}/{}", segments[n - 2], segments[n - 1])
    }
}

/// Coarse "5m" / "3h" / "2d" age from a millisecond delta. Clamps negatives
/// (clock skew) to "now".
fn relative_age(delta_ms: i64) -> String {
    if delta_ms <= 0 {
        return "now".to_string();
    }
    let secs = delta_ms / 1000;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days >= 1 {
        format!("{days}d")
    } else if hours >= 1 {
        format!("{hours}h")
    } else if mins >= 1 {
        format!("{mins}m")
    } else {
        "now".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(adapter: AgentAdapter, prompt: Option<&str>, cwd: Option<&str>, ts: Option<i64>) -> SessionEntry {
        SessionEntry {
            session_id: "id".into(),
            adapter,
            cwd: cwd.map(str::to_string),
            first_prompt: prompt.map(str::to_string),
            last_message_ts_ms: ts,
            entry_count: None,
        }
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
    fn row_label_includes_tag_and_prompt() {
        let e = entry(AgentAdapter::ClaudeCode, Some("refactor the parser"), None, None);
        assert_eq!(session_row_label(&e), "claude · refactor the parser");
    }

    #[test]
    fn row_label_falls_back_when_prompt_absent() {
        let e = entry(AgentAdapter::Codex, None, None, None);
        assert_eq!(session_row_label(&e), "codex · (no prompt)");
    }

    #[test]
    fn row_detail_combines_short_cwd_and_age() {
        let now = 10_000_000;
        let e = entry(
            AgentAdapter::ClaudeCode,
            Some("p"),
            Some("/Users/x/Code/oximux"),
            Some(now - 3 * 60 * 60 * 1000), // 3 hours ago
        );
        assert_eq!(session_row_detail(&e, now), "Code/oximux · 3h");
    }

    #[test]
    fn row_detail_without_timestamp_is_just_cwd() {
        let e = entry(AgentAdapter::ClaudeCode, Some("p"), Some("/a/b/c"), None);
        assert_eq!(session_row_detail(&e, 0), "b/c");
    }

    #[test]
    fn filter_sessions_ranks_by_label_and_excludes_non_matches() {
        let entries = vec![
            entry(AgentAdapter::ClaudeCode, Some("write tests"), None, None),
            entry(AgentAdapter::Codex, Some("refactor parser"), None, None),
        ];
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
    fn relative_age_buckets() {
        assert_eq!(relative_age(-5), "now");
        assert_eq!(relative_age(30 * 1000), "now");
        assert_eq!(relative_age(5 * 60 * 1000), "5m");
        assert_eq!(relative_age(2 * 60 * 60 * 1000), "2h");
        assert_eq!(relative_age(3 * 24 * 60 * 60 * 1000), "3d");
    }

    #[test]
    fn short_path_keeps_last_two_segments() {
        assert_eq!(short_path("/Users/x/Code/oximux"), "Code/oximux");
        assert_eq!(short_path("/a"), "a");
        assert_eq!(short_path("/a/b"), "a/b");
        assert_eq!(short_path("/a/b/c/"), "b/c");
    }
}
