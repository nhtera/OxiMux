//! Pure data layer for the agents dashboard — no GPUI runtime required.
//!
//! Provides `AgentRow` (one row per agent session across all projects),
//! `workspace_sort_tier` (priority tier for sorting), and `sort_agent_rows` (stable
//! sort by tier, then recency within a tier).
//!
//! Row assembly (`build_agent_rows`, `widest_row_index`) lives in the sibling
//! `row_builder` module; this file is the pure `AgentRow` model + ranking and
//! is testable without a window.

use oximux_agents::attention::Attention;
use oximux_agents::row_actions::RowActions;
use oximux_core::{AgentStatus, Workspace};

use crate::shell::agents_dashboard::sections::section_of;
use crate::shell::left_rail::RailAgentTarget;

/// One row in the agents dashboard — **one per agent session** (a workspace
/// with multiple agents yields multiple rows). Carries enough data to render
/// the card and to focus the agent on click — avoids re-querying any map or
/// borrowing a live channel inside the `uniform_list` closure.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// Human-readable project name for the repo badge.
    pub project_name: String,
    /// The owning workspace. Its `worktree_path` is the row identity; its
    /// `branch`/`slug` drive the branch badge.
    pub workspace: Workspace,
    /// Stable per-session row key (`agent_sessions.id` or `ambient:<pty>`) —
    /// the card's element id and the spinner animation key.
    pub db_id: String,
    /// What clicking this card focuses: the agent's tab (tracked) or its
    /// terminal (ambient).
    pub target: RailAgentTarget,
    /// Pre-formatted relative age ("now", "5m", "2d") computed at merge time.
    pub age_label: String,
    /// Effective status (live snapshot when live, else DB). `Some` for every
    /// session row; `Option` only so the sort tiers share one signature.
    pub status: Option<AgentStatus>,
    /// Whether a live tab/terminal currently backs this row.
    pub is_live: bool,
    /// Bundled SVG glyph for the agent's adapter, resolved once at build time.
    pub icon_path: &'static str,
    /// Recency key for the in-tier secondary sort: terminal `ended_at` else
    /// `started_at`, as the raw RFC-3339 string. `None` when unavailable.
    pub last_active_at: Option<String>,
    /// Adapter display name ("Claude Code") — the primary text when the agent
    /// has no prompt yet.
    pub label: String,
    /// The user's prompt (the agent's title), collapsed to one line. `None`
    /// when the agent has no captured prompt (primary falls back to `label`).
    pub primary: Option<String>,
    /// The live tool step ("Edit: foo.rs") or last reply, collapsed to one
    /// line. `None` when the row has neither (secondary falls back to the
    /// state label).
    pub secondary: Option<String>,
    /// Idle-with-reply: the agent finished a turn, so the card shows the
    /// emerald done-check instead of a grey idle dot.
    pub completed_turn: bool,
    /// What this row wants from a person, from the one evaluator
    /// (`oximux_agents::attention`), or `None` when it wants nothing.
    ///
    /// Resolved once at build time, like every other field here, so the render
    /// closure never asks the host anything. `None` also covers a row built
    /// without an evaluator, which only tests do.
    pub attention: Option<Attention>,
    /// What this row can answer without being opened, from the one action
    /// table (`oximux_agents::row_actions`), resolved in the same host pass
    /// as `attention` — so the buttons and the accent bar on one row are
    /// answering the same moment's question.
    pub actions: RowActions,
}

/// Whether a status raises something a person has to act on, by the one
/// mapping in `oximux_agents::attention`.
///
/// This replaces `needs_attention`, whose own list also counted `Done` and
/// `Interrupted` — so the nav badge used to bump for every clean finish and
/// for every session the last quit killed and the boot sweep stamped
/// `Interrupted`. A clean finish is now `FinishedUnread`, which is
/// informational and deliberately uncounted.
pub fn status_needs_you(status: &AgentStatus) -> bool {
    // `completed_turn` is false here: an unread finish is informational, so
    // it never changes this answer whichever way it is passed.
    oximux_agents::attention::kind_of_agent_status(status, false)
        .is_some_and(|(kind, _)| kind.is_needs_you())
}

/// Sort tier for a **workspace** row in the rail's Smart sort, from the
/// rolled-up status of the agents inside it.
///
/// Not an attention definition, and deliberately not called one any more: a
/// workspace is not a session, has no prefs of its own and nothing anyone can
/// answer. Phase 5 replaces this with the real roll-up — the worst state of
/// the subtree, ignoring children promoted into NEEDS YOU — at which point
/// this goes. Until then it keeps the rail's ordering exactly as it was.
///
/// Tiers:
///   0 — NeedsApproval / WaitingForInput  (user action required)
///   1 — Running                           (active work in progress)
///   2 — Idle / live without status        (ready, no action needed)
///   3 — Done (exit 0)                     (clean completion)
///   4 — Failed / Interrupted / Done (!0)  (terminal errors)
pub fn workspace_sort_tier(status: Option<&AgentStatus>, is_live: bool) -> u8 {
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

/// Sort `rows` by the section they will be cut into, then by recency (newest
/// `last_active_at` first) within each tier. `slice::sort_by` is stable, so
/// rows that tie on both keys keep their input order. A row with no timestamp
/// sorts after rows that have one.
///
/// The key is `section_of(row).index()` rather than a second tier table. The
/// dashboard sorts and *then* groups, so two tables could disagree about which
/// row is more urgent and the list would come out shuffled inside its own
/// headers.
pub fn sort_agent_rows(rows: &mut [AgentRow]) {
    // Worst alert first within a section. `HostCore::needs_you` hands the
    // chip and the hotkey cycle the same order, so the rows the eye walks and
    // the rows the hotkey walks are the same rows in the same order; without
    // it a failure could sit above the permission that is actually blocking.
    let urgency = |row: &AgentRow| {
        row.attention.as_ref().map_or(u8::MAX, |attention| attention.kind.priority())
    };
    rows.sort_by(|a, b| {
        section_of(a)
            .index()
            .cmp(&section_of(b).index())
            .then_with(|| urgency(a).cmp(&urgency(b)))
            // Descending recency: greater (newer / `Some` > `None`) RFC-3339
            // string first, so compare b against a.
            .then_with(|| b.last_active_at.cmp(&a.last_active_at))
    });
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, project_id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: project_id.to_string(),
            // Not a branch OxiMux minted: a synthesized row or a
            // fixture. `false` is the reading that never deletes.
            branch_minted: false,
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
            comment: String::new(),
            phase: String::new(),
        }
    }

    #[test]
    fn the_badge_counts_only_what_somebody_has_to_act_on() {
        assert!(status_needs_you(&AgentStatus::NeedsApproval("x".into())));
        assert!(status_needs_you(&AgentStatus::WaitingForInput));
        assert!(status_needs_you(&AgentStatus::Failed("boom".into())));
        assert!(status_needs_you(&AgentStatus::Done { code: Some(1) }));
        assert!(status_needs_you(&AgentStatus::Done { code: None }));
        assert!(!status_needs_you(&AgentStatus::Running));
        assert!(!status_needs_you(&AgentStatus::Idle));
    }

    #[test]
    fn the_badge_no_longer_counts_a_clean_finish_or_a_killed_session() {
        // Both counted under the old `needs_attention`, and both were wrong.
        // A clean finish is unread work, which is informational and
        // deliberately uncounted; `Interrupted` is stamped on every session
        // the last quit killed, so counting it put the whole of the previous
        // run back on the badge at every start.
        assert!(
            !status_needs_you(&AgentStatus::Done { code: Some(0) }),
            "a clean finish is FinishedUnread: shown, not counted"
        );
        assert!(
            !status_needs_you(&AgentStatus::Interrupted),
            "a boot-swept session is not an alert"
        );
    }

    // ── workspace_sort_tier tiers ──────────────────────────────────────────────────

    #[test]
    fn tier_zero_needs_approval() {
        assert_eq!(
            workspace_sort_tier(Some(&AgentStatus::NeedsApproval("x".into())), false),
            0
        );
    }

    #[test]
    fn tier_zero_waiting_for_input() {
        assert_eq!(workspace_sort_tier(Some(&AgentStatus::WaitingForInput), false), 0);
    }

    #[test]
    fn tier_one_running() {
        assert_eq!(workspace_sort_tier(Some(&AgentStatus::Running), false), 1);
    }

    #[test]
    fn tier_two_idle() {
        assert_eq!(workspace_sort_tier(Some(&AgentStatus::Idle), false), 2);
    }

    #[test]
    fn tier_two_live_without_status() {
        assert_eq!(workspace_sort_tier(None, true), 2);
    }

    #[test]
    fn tier_two_idle_and_live() {
        assert_eq!(workspace_sort_tier(Some(&AgentStatus::Idle), true), 2);
    }

    #[test]
    fn tier_three_done_clean() {
        assert_eq!(
            workspace_sort_tier(Some(&AgentStatus::Done { code: Some(0) }), false),
            3
        );
    }

    #[test]
    fn tier_four_done_nonzero() {
        assert_eq!(
            workspace_sort_tier(Some(&AgentStatus::Done { code: Some(1) }), false),
            4
        );
    }

    #[test]
    fn tier_four_done_unknown_code() {
        assert_eq!(
            workspace_sort_tier(Some(&AgentStatus::Done { code: None }), false),
            4
        );
    }

    #[test]
    fn tier_four_failed() {
        assert_eq!(
            workspace_sort_tier(Some(&AgentStatus::Failed("boom".into())), false),
            4
        );
    }

    #[test]
    fn tier_four_interrupted() {
        assert_eq!(workspace_sort_tier(Some(&AgentStatus::Interrupted), false), 4);
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
        let for_attention = status.clone();
        AgentRow {
            project_name: "P".to_string(),
            workspace: workspace(id, "proj"),
            db_id: id.to_string(),
            target: RailAgentTarget::AgentSession {
                db_id: id.to_string(),
            },
            age_label: String::new(),
            status,
            is_live,
            icon_path: "icons/sparkles.svg",
            last_active_at: last_active_at.map(str::to_string),
            label: "Claude Code".to_string(),
            primary: None,
            secondary: None,
            completed_turn: false,
            attention: unseen_attention(for_attention.as_ref()),
            // A cockpit row has no registry handle, so the one table offers
            // it nothing to answer beyond a Dismiss — which these ordering
            // tests do not read. Built from the same attention so the row is
            // not internally inconsistent.
            actions: oximux_agents::row_actions::row_actions(
                unseen_attention(for_attention.as_ref()).map(|a| a.kind),
                &[],
                None,
            ),
        }
    }

    /// A row's attention the way the app resolves a cockpit row's: the one
    /// mapping, the one evaluator, nobody having looked at anything. Stubbed
    /// to `None`, the ordering tests below would pass while the sections they
    /// are the sort key for were wrong.
    fn unseen_attention(status: Option<&AgentStatus>) -> Option<Attention> {
        let item = oximux_agents::attention::external_of_agent_status(
            "t".into(),
            "t".into(),
            status?,
            false,
            |_, _| 0,
        )?;
        oximux_agents::attention::of_external(
            "t",
            &oximux_agents::session_prefs::SessionPrefs::default(),
            &[item],
            &oximux_agents::attention::NothingSeen,
        )
    }

    #[test]
    fn sort_puts_needs_approval_before_running() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row("b", Some(AgentStatus::NeedsApproval("x".into())), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].db_id, "b");
        assert_eq!(rows[1].db_id, "a");
    }

    #[test]
    fn sort_puts_running_before_idle() {
        let mut rows = vec![
            make_row("idle", Some(AgentStatus::Idle), false),
            make_row("run", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].db_id, "run");
    }

    #[test]
    fn sort_puts_an_unread_finish_before_idle() {
        // This inverts, deliberately. The old rank put Idle (tier 2) above a
        // clean finish (tier 3) while `section_of` put DONE above IDLE, so the
        // list was sorted by one order and then cut into headers by another —
        // rows came out shuffled inside their own sections. There is one key
        // now, and an unread finish is work waiting to be read.
        let mut rows = vec![
            make_row("idle", Some(AgentStatus::Idle), false),
            make_row("done", Some(AgentStatus::Done { code: Some(0) }), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].db_id, "done");
        assert_eq!(rows[1].db_id, "idle");
    }

    #[test]
    fn sort_is_stable_within_tier() {
        let mut rows = vec![
            make_row("first", Some(AgentStatus::Running), false),
            make_row("second", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].db_id, "first");
        assert_eq!(rows[1].db_id, "second");
    }

    #[test]
    fn sort_running_rows_by_recency_descending() {
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
        assert_eq!(rows[0].db_id, "newer");
        assert_eq!(rows[1].db_id, "older");
    }

    #[test]
    fn approval_row_floats_above_newer_running() {
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
        assert_eq!(rows[0].db_id, "approval-older");
        assert_eq!(rows[1].db_id, "running-newer");
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
        let ids: Vec<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        // NEEDS YOU first and worst-first inside it, so the waiting prompt (d)
        // comes before the failure (b); then the unread finish (a), then
        // working (c), then idle (e). Under the old tiers a clean finish sat
        // below idle and a failure sat below both, in ENDED.
        assert_eq!(ids, vec!["d", "b", "a", "c", "e"]);
    }
}
