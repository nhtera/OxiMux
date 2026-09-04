//! Filtering for the agents dashboard — the `Filter…` text box + `Status`
//! control from the reference cockpit. Pure + testable: `apply_filter` narrows
//! the per-session rows by the query text AND by status bucket, before they are
//! grouped into sections. The query is whitespace-tokenized and every token must
//! appear (case-insensitive) somewhere in a row's combined searchable text
//! (prompt / adapter / project / branch / slug / reply) — order-independent AND,
//! so a multi-word query like `oximux main` can span fields.

use crate::shell::agents_dashboard::model::AgentRow;
use crate::shell::agents_dashboard::sections::{SectionKind, section_of};

/// The status the dashboard is narrowed to. `All` is the passthrough default;
/// the rest mirror the section buckets.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StatusFilter {
    #[default]
    All,
    Attention,
    Working,
    Done,
    Idle,
    Ended,
}

impl StatusFilter {
    /// Every bucket, in menu order (`All` first) — the dropdown's row list and
    /// the source of `next`'s cycle.
    pub const ALL: [StatusFilter; 6] = [
        StatusFilter::All,
        StatusFilter::Attention,
        StatusFilter::Working,
        StatusFilter::Done,
        StatusFilter::Idle,
        StatusFilter::Ended,
    ];

    /// The next filter in the cycle (wraps `Ended → All`).
    pub fn next(self) -> StatusFilter {
        let i = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// Chip label.
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::Attention => "Needs attention",
            StatusFilter::Working => "Working",
            StatusFilter::Done => "Done",
            StatusFilter::Idle => "Idle",
            StatusFilter::Ended => "Ended",
        }
    }

    /// The section this filter restricts to, or `None` for `All`.
    fn section(self) -> Option<SectionKind> {
        match self {
            StatusFilter::All => None,
            StatusFilter::Attention => Some(SectionKind::Attention),
            StatusFilter::Working => Some(SectionKind::Working),
            StatusFilter::Done => Some(SectionKind::Done),
            StatusFilter::Idle => Some(SectionKind::Idle),
            StatusFilter::Ended => Some(SectionKind::Ended),
        }
    }

    /// Whether `row` passes this status filter.
    fn matches(self, row: &AgentRow) -> bool {
        match self.section() {
            None => true,
            Some(kind) => section_of(row) == kind,
        }
    }
}

/// Narrow `rows` by the query text AND the status filter. Empty `text` + `All`
/// is a passthrough. The text is whitespace-tokenized and every token must
/// appear somewhere in the row's searchable fields (order-independent AND),
/// matching the reference cockpit's session filter.
pub fn apply_filter(rows: Vec<AgentRow>, text: &str, status: StatusFilter) -> Vec<AgentRow> {
    // Tokenize once, up front: every row reuses the same lowercased token list.
    let tokens = query_tokens(text);
    rows.into_iter()
        .filter(|r| status.matches(r) && matches_tokens(r, &tokens))
        .collect()
}

/// Split a query into lowercased whitespace tokens. Empty / whitespace-only
/// input yields no tokens (a passthrough match).
fn query_tokens(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_lowercase).collect()
}

/// True when **every** token appears in the row's combined searchable text
/// (case-insensitive substring). No tokens → matches everything. Joining the
/// fields into one haystack lets a multi-word query span fields — e.g.
/// `"oximux main"` matches a row whose project is `oximux` and branch `main`,
/// which a per-field whole-string match would miss.
fn matches_tokens(row: &AgentRow, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let haystack = searchable_text(row);
    tokens.iter().all(|t| haystack.contains(t.as_str()))
}

/// The lowercased, space-joined searchable text for a row: prompt, adapter
/// label, project name, branch, slug, and the live reply/tool line.
fn searchable_text(row: &AgentRow) -> String {
    let mut parts: Vec<&str> = vec![
        &row.label,
        &row.project_name,
        &row.workspace.branch,
        &row.workspace.slug,
    ];
    if let Some(p) = row.primary.as_deref() {
        parts.push(p);
    }
    if let Some(s) = row.secondary.as_deref() {
        parts.push(s);
    }
    parts.join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::left_rail::RailAgentTarget;
    use oximux_core::{AgentStatus, Workspace};

    fn workspace(branch: &str, slug: &str) -> Workspace {
        Workspace {
            id: "ws".into(),
            project_id: "p".into(),
            name: "ws".into(),
            slug: slug.into(),
            branch: branch.into(),
            worktree_path: "/tmp/ws".into(),
            status: "active".into(),
            created_at: "2026-06-01T00:00:00Z".into(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    fn row(
        id: &str,
        status: AgentStatus,
        project: &str,
        branch: &str,
        primary: Option<&str>,
    ) -> AgentRow {
        AgentRow {
            project_name: project.into(),
            workspace: workspace(branch, "slug"),
            db_id: id.into(),
            target: RailAgentTarget::AgentSession { db_id: id.into() },
            age_label: String::new(),
            status: Some(status),
            is_live: false,
            icon_path: "icons/sparkles.svg",
            last_active_at: None,
            label: "Claude Code".into(),
            primary: primary.map(str::to_string),
            secondary: None,
            completed_turn: false,
        }
    }

    #[test]
    fn status_cycle_wraps() {
        assert_eq!(StatusFilter::All.next(), StatusFilter::Attention);
        assert_eq!(StatusFilter::Ended.next(), StatusFilter::All);
    }

    #[test]
    fn all_and_empty_text_is_passthrough() {
        let rows = vec![
            row("a", AgentStatus::Running, "oximux", "main", Some("hi")),
            row("b", AgentStatus::Idle, "graphify", "v4", None),
        ];
        let out = apply_filter(rows, "", StatusFilter::All);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn status_filter_keeps_only_matching_section() {
        let rows = vec![
            row("run", AgentStatus::Running, "p", "b", None),
            row("idle", AgentStatus::Idle, "p", "b", None),
        ];
        let out = apply_filter(rows, "", StatusFilter::Working);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].db_id, "run");
    }

    #[test]
    fn text_matches_prompt_project_and_branch() {
        let rows = vec![
            row("a", AgentStatus::Running, "oximux", "main", Some("fix the parser")),
            row("b", AgentStatus::Running, "graphify", "v4", Some("write docs")),
        ];
        // prompt substring
        assert_eq!(apply_filter(rows.clone(), "parser", StatusFilter::All).len(), 1);
        // project substring (case-insensitive)
        assert_eq!(apply_filter(rows.clone(), "GRAPHIFY", StatusFilter::All).len(), 1);
        // branch substring
        assert_eq!(apply_filter(rows, "main", StatusFilter::All).len(), 1);
    }

    #[test]
    fn text_and_status_combine() {
        let rows = vec![
            row("a", AgentStatus::Running, "oximux", "main", Some("fix parser")),
            row("b", AgentStatus::Idle, "oximux", "main", Some("fix parser")),
        ];
        // "fix" matches both, but Working keeps only the running one.
        let out = apply_filter(rows, "fix", StatusFilter::Working);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].db_id, "a");
    }

    #[test]
    fn no_match_yields_empty() {
        let rows = vec![row("a", AgentStatus::Running, "oximux", "main", Some("hi"))];
        assert!(apply_filter(rows, "zzz-nope", StatusFilter::All).is_empty());
    }

    #[test]
    fn multi_token_query_spans_fields_order_independent() {
        // "oximux main" — project token + branch token — must match even though
        // no single field contains the literal "oximux main" (the per-field
        // whole-string match this replaced would return zero rows here).
        let rows = vec![
            row("a", AgentStatus::Running, "oximux", "main", Some("fix parser")),
            row("b", AgentStatus::Running, "graphify", "v4", Some("fix parser")),
        ];
        assert_eq!(apply_filter(rows.clone(), "oximux main", StatusFilter::All).len(), 1);
        // Order-independent: branch token first, project token second.
        assert_eq!(apply_filter(rows, "main oximux", StatusFilter::All).len(), 1);
    }

    #[test]
    fn all_tokens_must_match_and_semantics() {
        let rows = vec![row(
            "a",
            AgentStatus::Running,
            "oximux",
            "main",
            Some("fix the parser"),
        )];
        // Both tokens present across fields → match.
        assert_eq!(apply_filter(rows.clone(), "parser oximux", StatusFilter::All).len(), 1);
        // One token absent → rejected (AND, not OR).
        assert!(apply_filter(rows, "parser nope", StatusFilter::All).is_empty());
    }

    #[test]
    fn extra_whitespace_between_tokens_is_ignored() {
        let rows = vec![row("a", AgentStatus::Running, "oximux", "main", Some("hi"))];
        assert_eq!(
            apply_filter(rows, "  oximux   main  ", StatusFilter::All).len(),
            1
        );
    }
}
