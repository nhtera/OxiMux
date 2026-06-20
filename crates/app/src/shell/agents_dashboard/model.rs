//! Pure data layer for the agents dashboard — no GPUI runtime required.
//!
//! Provides `AgentRow` (one row per live/status-bearing workspace across all
//! projects), `attention_rank` (priority tier for sorting), and
//! `sort_agent_rows` (stable sort by tier, then recency within a tier).
//!
//! Row assembly (`build_agent_rows`, `widest_row_index`) lives in the sibling
//! `row_builder` module; this file is the pure `AgentRow` model + ranking and
//! is testable without a window.

use oximux_core::{AgentStatus, Workspace};

use crate::shell::agent_presentation::AgentVerb;
use crate::shell::left_rail::workspace_row::DiffCounts;

/// One row in the agents dashboard. Carries enough data to render the row
/// and to activate the workspace on click — avoids re-querying any map
/// inside the `uniform_list` closure.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// Human-readable project name for the first-column label.
    pub project_name: String,
    /// The workspace this row represents. Its `id` and `worktree_path` drive
    /// the click handler; its `name`/`slug`/`branch` drive the row label.
    pub workspace: Workspace,
    /// Resolved verb label + color (reused from `agent_presentation`).
    pub verb: AgentVerb,
    /// Working-tree diff counts. `None` when not yet cached or unavailable.
    pub diff: Option<DiffCounts>,
    /// Latest agent-session status. `None` for live-but-never-started workspaces.
    pub status: Option<AgentStatus>,
    /// Whether the workspace currently has an open agent tab (green idle dot).
    pub is_live: bool,
    /// Live tool-activity line for Running sessions ("Bash: cargo test…"),
    /// tailed from the agent CLI's session log on a background tick. `None`
    /// when idle, non-Running, or the log has no fresh tool call.
    pub activity: Option<String>,
    /// Bundled SVG glyph for the workspace's latest agent adapter
    /// (`icons/claude-code.svg` etc.), resolved once at build time so the
    /// render closure never re-derives it. Falls back to the sparkles glyph.
    pub icon_path: &'static str,
    /// Recency key for the in-tier secondary sort: the latest session's
    /// `ended_at` (terminal) else `started_at`, as the raw RFC-3339 string.
    /// Storage stamps every value with `Utc::now().to_rfc3339()` (a `+00:00`
    /// suffix, NOT `Z`), so all values share one format and lexicographic
    /// ordering matches chronological — no parse to millis needed. `None`
    /// for a live-but-never-started workspace.
    pub last_active_at: Option<String>,
}

/// True for states a user should come look at: the session is blocked on
/// them (approval / input) or has stopped (done / failed / interrupted) —
/// the boolean companion of `attention_rank`'s tiers 0/3/4. Drives the
/// unread badge on the Agents nav row.
pub fn needs_attention(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::NeedsApproval(_)
            | AgentStatus::WaitingForInput
            | AgentStatus::Done { .. }
            | AgentStatus::Failed(_)
            | AgentStatus::Interrupted
    )
}

/// Attention tier for sorting. Lower = higher priority (floats to top).
///
/// Tiers:
///   0 — NeedsApproval / WaitingForInput  (user action required)
///   1 — Running                           (active work in progress)
///   2 — Idle / live without status        (ready, no action needed)
///   3 — Done (exit 0)                     (clean completion)
///   4 — Failed / Interrupted / Done (!0)  (terminal errors)
pub fn attention_rank(status: Option<&AgentStatus>, is_live: bool) -> u8 {
    match status {
        Some(AgentStatus::NeedsApproval(_)) | Some(AgentStatus::WaitingForInput) => 0,
        Some(AgentStatus::Running) => 1,
        // No status but live: treat same as Idle/live (tier 2).
        None if is_live => 2,
        Some(AgentStatus::Idle) => 2,
        Some(AgentStatus::Done { code: Some(0) }) => 3,
        // Non-zero exit, unknown exit, Failed, Interrupted, dormant.
        Some(AgentStatus::Done { .. })
        | Some(AgentStatus::Failed(_))
        | Some(AgentStatus::Interrupted) => 4,
        // Dormant workspace with no status and not live would be filtered
        // before reaching here, but map to lowest priority defensively.
        None => 4,
    }
}

/// Sort `rows` by `attention_rank`, then by recency (newest `last_active_at`
/// first) within each tier. `slice::sort_by` is stable, so rows that tie on
/// both keys (e.g. two running agents with no timestamp) keep their input
/// order. A row with no timestamp sorts after rows that have one.
pub fn sort_agent_rows(rows: &mut [AgentRow]) {
    rows.sort_by(|a, b| {
        attention_rank(a.status.as_ref(), a.is_live)
            .cmp(&attention_rank(b.status.as_ref(), b.is_live))
            // Descending recency: greater (newer / `Some` > `None`) RFC-3339
            // string first, so compare b against a.
            .then_with(|| b.last_active_at.cmp(&a.last_active_at))
    });
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::agent_presentation::agent_verb;
    use oximux_settings::Theme;

    fn t() -> Theme {
        Theme::charcoal()
    }

    fn workspace(id: &str, project_id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: format!("ws-{id}"),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: format!("/tmp/{project_id}/{id}"),
            status: "active".to_string(),
            created_at: "2026-06-01T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
        }
    }

    #[test]
    fn needs_attention_matches_blocked_and_terminal_states() {
        // Blocked-on-user and stopped states badge; in-flight states don't.
        assert!(needs_attention(&AgentStatus::NeedsApproval("x".into())));
        assert!(needs_attention(&AgentStatus::WaitingForInput));
        assert!(needs_attention(&AgentStatus::Done { code: Some(0) }));
        assert!(needs_attention(&AgentStatus::Done { code: None }));
        assert!(needs_attention(&AgentStatus::Failed("boom".into())));
        assert!(needs_attention(&AgentStatus::Interrupted));
        assert!(!needs_attention(&AgentStatus::Running));
        assert!(!needs_attention(&AgentStatus::Idle));
    }

    // ── attention_rank tiers ──────────────────────────────────────────────────

    #[test]
    fn tier_zero_needs_approval() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::NeedsApproval("x".into())), false),
            0
        );
    }

    #[test]
    fn tier_zero_waiting_for_input() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::WaitingForInput), false),
            0
        );
    }

    #[test]
    fn tier_one_running() {
        assert_eq!(attention_rank(Some(&AgentStatus::Running), false), 1);
    }

    #[test]
    fn tier_two_idle() {
        assert_eq!(attention_rank(Some(&AgentStatus::Idle), false), 2);
    }

    #[test]
    fn tier_two_live_without_status() {
        // A workspace with an open agent tab but no concrete status → tier 2.
        assert_eq!(attention_rank(None, true), 2);
    }

    #[test]
    fn tier_two_idle_and_live() {
        // Idle + live (the green "Ready" case) stays tier 2 — the live flag
        // must not promote it above Running.
        assert_eq!(attention_rank(Some(&AgentStatus::Idle), true), 2);
    }

    #[test]
    fn tier_three_done_clean() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: Some(0) }), false),
            3
        );
    }

    #[test]
    fn tier_four_done_nonzero() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: Some(1) }), false),
            4
        );
    }

    #[test]
    fn tier_four_done_unknown_code() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: None }), false),
            4
        );
    }

    #[test]
    fn tier_four_failed() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Failed("boom".into())), false),
            4
        );
    }

    #[test]
    fn tier_four_interrupted() {
        assert_eq!(attention_rank(Some(&AgentStatus::Interrupted), false), 4);
    }

    // ── sort_agent_rows — tier ordering + recency ─────────────────────────────

    fn make_row(id: &str, status: Option<AgentStatus>, is_live: bool) -> AgentRow {
        make_row_at(id, status, is_live, None)
    }

    fn make_row_at(
        id: &str,
        status: Option<AgentStatus>,
        is_live: bool,
        last_active_at: Option<&str>,
    ) -> AgentRow {
        let theme = t();
        let verb = agent_verb(status.as_ref(), is_live, theme);
        AgentRow {
            project_name: "P".to_string(),
            workspace: workspace(id, "proj"),
            verb,
            diff: None,
            status,
            is_live,
            activity: None,
            icon_path: "icons/sparkles.svg",
            last_active_at: last_active_at.map(str::to_string),
        }
    }

    #[test]
    fn sort_puts_needs_approval_before_running() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row("b", Some(AgentStatus::NeedsApproval("x".into())), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "b");
        assert_eq!(rows[1].workspace.id, "a");
    }

    #[test]
    fn sort_puts_waiting_before_running() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row("b", Some(AgentStatus::WaitingForInput), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "b");
    }

    #[test]
    fn sort_puts_running_before_idle() {
        let mut rows = vec![
            make_row("idle", Some(AgentStatus::Idle), false),
            make_row("run", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "run");
    }

    #[test]
    fn sort_puts_idle_before_done_clean() {
        let mut rows = vec![
            make_row("done", Some(AgentStatus::Done { code: Some(0) }), false),
            make_row("idle", Some(AgentStatus::Idle), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "idle");
    }

    #[test]
    fn sort_puts_done_clean_before_failed() {
        let mut rows = vec![
            make_row("fail", Some(AgentStatus::Failed("x".into())), false),
            make_row("done", Some(AgentStatus::Done { code: Some(0) }), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "done");
    }

    #[test]
    fn sort_is_stable_within_tier() {
        // Two running agents, no timestamps — input order must be preserved.
        let mut rows = vec![
            make_row("first", Some(AgentStatus::Running), false),
            make_row("second", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "first");
        assert_eq!(rows[1].workspace.id, "second");
    }

    #[test]
    fn sort_running_rows_by_recency_descending() {
        // SC1: within the same attention tier, the newer session floats up.
        let mut rows = vec![
            make_row_at(
                "older",
                Some(AgentStatus::Running),
                false,
                Some("2026-06-19T08:00:00+00:00"),
            ),
            make_row_at(
                "newer",
                Some(AgentStatus::Running),
                false,
                Some("2026-06-21T08:00:00+00:00"),
            ),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "newer");
        assert_eq!(rows[1].workspace.id, "older");
    }

    #[test]
    fn sort_row_with_timestamp_floats_above_one_without() {
        // Within a tier, a row carrying a recency timestamp beats one missing it.
        let mut rows = vec![
            make_row_at("no-ts", Some(AgentStatus::Running), false, None),
            make_row_at(
                "has-ts",
                Some(AgentStatus::Running),
                false,
                Some("2026-06-20T00:00:00+00:00"),
            ),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "has-ts");
    }

    #[test]
    fn approval_row_floats_above_newer_running() {
        // SC2: tier-0 (NeedsApproval) beats tier-1 (Running) regardless of a
        // newer running timestamp — the attention tier is the primary key.
        let mut rows = vec![
            make_row_at(
                "running-newer",
                Some(AgentStatus::Running),
                false,
                Some("2026-06-21T12:00:00+00:00"),
            ),
            make_row_at(
                "approval-older",
                Some(AgentStatus::NeedsApproval("x".into())),
                false,
                Some("2026-06-10T00:00:00+00:00"),
            ),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "approval-older");
        assert_eq!(rows[1].workspace.id, "running-newer");
    }

    #[test]
    fn sort_full_mixed_list() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Done { code: Some(0) }), false),
            make_row("b", Some(AgentStatus::Failed("x".into())), false),
            make_row("c", Some(AgentStatus::Running), false),
            make_row("d", Some(AgentStatus::WaitingForInput), false),
            make_row("e", Some(AgentStatus::Idle), false),
        ];
        sort_agent_rows(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|r| r.workspace.id.as_str()).collect();
        // Expected order: d (0), c (1), e (2), a (3), b (4)
        assert_eq!(ids, vec!["d", "c", "e", "a", "b"]);
    }
}
