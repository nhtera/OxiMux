//! Status-section grouping for the agents dashboard — the `DONE 4`-style
//! headers from the reference cockpit. Pure + testable: turns an
//! attention-sorted `Vec<AgentRow>` into a flat `Header | Row` item list,
//! severity-ordered, empty sections dropped. The render layer (mod.rs) walks
//! the items and paints a header or a card per entry.

use oximux_agents::attention::AttentionKind;
use oximux_core::AgentStatus;

use crate::shell::agents_dashboard::model::AgentRow;

/// A status section, in severity order (top of the list first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionKind {
    /// Someone has to act: a permission, a question, a failure, a stall —
    /// [`AttentionKind::is_needs_you`] and nothing else.
    Attention,
    /// Finished a turn that nobody has read yet.
    FinishedUnread,
    /// Actively running. A rate-limited session lives here too: it retries
    /// itself, so it reads as working with the limit as its reason.
    Working,
    /// Finished and already looked at.
    Done,
    /// Ready, never run a turn.
    Idle,
    /// Interrupted, or a failure that has been dealt with.
    Ended,
}

impl SectionKind {
    /// Severity order — drives both the bucket index and the render order.
    pub const ORDER: [SectionKind; 6] = [
        SectionKind::Attention,
        SectionKind::FinishedUnread,
        SectionKind::Working,
        SectionKind::Done,
        SectionKind::Idle,
        SectionKind::Ended,
    ];

    /// Position in [`SectionKind::ORDER`] (the bucket index).
    ///
    /// Public because it is also the row sort key: `sort_agent_rows` orders by
    /// it, so the sorted list and the sections it is cut into cannot disagree
    /// about which row is more urgent.
    pub fn index(self) -> usize {
        match self {
            SectionKind::Attention => 0,
            SectionKind::FinishedUnread => 1,
            SectionKind::Working => 2,
            SectionKind::Done => 3,
            SectionKind::Idle => 4,
            SectionKind::Ended => 5,
        }
    }

    /// Small-caps header label.
    pub fn label(self) -> &'static str {
        match self {
            SectionKind::Attention => "NEEDS YOU",
            SectionKind::FinishedUnread => "FINISHED · UNREAD",
            SectionKind::Working => "WORKING",
            SectionKind::Done => "DONE",
            SectionKind::Idle => "IDLE",
            SectionKind::Ended => "ENDED",
        }
    }
}

/// Which section a row belongs to.
///
/// The top two sections are the evaluator's answer, not this function's: a row
/// is in NEEDS YOU because [`AttentionKind::is_needs_you`] said so, and in
/// FINISHED · UNREAD because the evaluator weighed the finish against what the
/// person has already looked at. Below that there is no attention left to
/// read, so the row's own lifecycle sections it — a finished-turn row
/// (idle + reply) is "done" even though its raw status is Idle, matching the
/// card's check.
///
/// That split is the point. A failure sits in NEEDS YOU until it is viewed or
/// dismissed and then falls to ENDED, and a stall returns to WORKING, without
/// either section having to know the dismissal rules.
pub fn section_of(row: &AgentRow) -> SectionKind {
    if let Some(attention) = &row.attention {
        if attention.kind.is_needs_you() {
            return SectionKind::Attention;
        }
        if attention.kind == AttentionKind::FinishedUnread {
            return SectionKind::FinishedUnread;
        }
        // `Limited` is informational and retries itself, so it falls through
        // to the status and reads as working.
    }
    if row.completed_turn {
        return SectionKind::Done;
    }
    match row.status.as_ref() {
        Some(AgentStatus::Running) => SectionKind::Working,
        Some(AgentStatus::Done { code: Some(0) }) => SectionKind::Done,
        Some(AgentStatus::Idle) | None => SectionKind::Idle,
        Some(AgentStatus::Done { .. })
        | Some(AgentStatus::Failed(_))
        | Some(AgentStatus::Interrupted) => SectionKind::Ended,
        // A blocked status with no attention of its own. Unreachable for a row
        // the evaluator has seen, because a prompt is not dismissible and so
        // never reads as dealt with; it happens only for a row built without
        // an evaluator. Showing a blocked agent as "working" would be a lie,
        // and this is the one reading that never tells it.
        Some(AgentStatus::NeedsApproval(_)) | Some(AgentStatus::WaitingForInput) => {
            SectionKind::Attention
        }
    }
}

/// One entry in the rendered dashboard: a section header or an agent card.
/// The row is boxed — `AgentRow` is large, so an unboxed variant would bloat
/// every header entry too.
pub enum DashboardItem {
    Header { kind: SectionKind, count: usize },
    Row(Box<AgentRow>),
}

/// Group attention-sorted `rows` into severity sections, emitting a header
/// followed by its rows for each non-empty section. Rows keep their incoming
/// (recency) order within a section.
pub fn group_into_sections(rows: Vec<AgentRow>) -> Vec<DashboardItem> {
    let mut buckets: [Vec<AgentRow>; SectionKind::ORDER.len()] =
        std::array::from_fn(|_| Vec::new());
    for row in rows {
        buckets[section_of(&row).index()].push(row);
    }
    let mut items = Vec::new();
    for (kind, bucket) in SectionKind::ORDER.into_iter().zip(buckets) {
        if bucket.is_empty() {
            continue;
        }
        items.push(DashboardItem::Header {
            kind,
            count: bucket.len(),
        });
        items.extend(bucket.into_iter().map(|r| DashboardItem::Row(Box::new(r))));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::left_rail::RailAgentTarget;
    use oximux_core::Workspace;

    fn workspace() -> Workspace {
        Workspace {
            id: "ws".into(),
            project_id: "p".into(),
            // Not a branch OxiMux minted: a synthesized row or a
            // fixture. `false` is the reading that never deletes.
            branch_minted: false,
            name: "ws".into(),
            slug: "ws".into(),
            branch: "main".into(),
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

    /// A row's attention the way the app resolves a cockpit row's: the one
    /// mapping, the one evaluator, nobody having looked at anything. These
    /// tests are about which section the evaluator's answer lands in, so a
    /// stub would make every one of them vacuous.
    fn unseen_attention(
        status: &AgentStatus,
        completed_turn: bool,
    ) -> Option<oximux_agents::attention::Attention> {
        let item = oximux_agents::attention::external_of_agent_status(
            "t".into(),
            "t".into(),
            status,
            completed_turn,
            |_, _| 0,
        )?;
        oximux_agents::attention::of_external(
            "t",
            &oximux_agents::session_prefs::SessionPrefs::default(),
            &[item],
            &oximux_agents::attention::NothingSeen,
        )
    }

    fn row(id: &str, status: AgentStatus, completed_turn: bool) -> AgentRow {
        let attention = unseen_attention(&status, completed_turn);
        AgentRow {
            project_name: "P".into(),
            workspace: workspace(),
            db_id: id.into(),
            target: RailAgentTarget::AgentSession { db_id: id.into() },
            age_label: String::new(),
            status: Some(status),
            is_live: false,
            icon_path: "icons/sparkles.svg",
            last_active_at: None,
            label: "Claude Code".into(),
            primary: None,
            secondary: None,
            completed_turn,
            attention,
            // Sectioning reads the attention's kind, never the buttons.
            actions: oximux_agents::row_actions::RowActions::nothing(),
        }
    }

    /// The same row with nobody's attention on it: a failure that has been
    /// viewed, or a row built before the evaluator saw it.
    fn row_without_attention(id: &str, status: AgentStatus, completed_turn: bool) -> AgentRow {
        AgentRow { attention: None, ..row(id, status, completed_turn) }
    }

    #[test]
    fn section_of_buckets_each_status() {
        assert_eq!(
            section_of(&row("a", AgentStatus::NeedsApproval("x".into()), false)),
            SectionKind::Attention
        );
        assert_eq!(
            section_of(&row("b", AgentStatus::WaitingForInput, false)),
            SectionKind::Attention
        );
        assert_eq!(
            section_of(&row("c", AgentStatus::Running, false)),
            SectionKind::Working
        );
        assert_eq!(
            section_of(&row("e", AgentStatus::Idle, false)),
            SectionKind::Idle
        );
        assert_eq!(
            section_of(&row("g", AgentStatus::Interrupted, false)),
            SectionKind::Ended,
            "a cancel asks nobody anything and stays where it was"
        );
    }

    #[test]
    fn a_failure_needs_you_until_it_has_been_dealt_with() {
        // The move this phase is for: a failure used to be filed under ENDED,
        // where nobody looked. It is an alert while it is unanswered now, and
        // falls to ENDED only once the evaluator says it has been seen.
        assert_eq!(
            section_of(&row("f", AgentStatus::Failed("boom".into()), false)),
            SectionKind::Attention
        );
        assert_eq!(
            section_of(&row_without_attention("f", AgentStatus::Failed("boom".into()), false)),
            SectionKind::Ended,
            "viewed or dismissed, it leaves NEEDS YOU without its status changing"
        );
    }

    #[test]
    fn an_unread_finish_has_its_own_section_and_a_read_one_is_done() {
        assert_eq!(
            section_of(&row("d", AgentStatus::Done { code: Some(0) }, false)),
            SectionKind::FinishedUnread,
            "a clean finish nobody has read"
        );
        assert_eq!(
            section_of(&row_without_attention("d", AgentStatus::Done { code: Some(0) }, false)),
            SectionKind::Done,
            "read, so it drops to the read-finished bucket"
        );
    }

    #[test]
    fn a_non_zero_exit_needs_you_rather_than_reading_as_finished() {
        assert_eq!(
            section_of(&row("x", AgentStatus::Done { code: Some(137) }, false)),
            SectionKind::Attention
        );
    }

    #[test]
    fn a_blocked_row_the_evaluator_never_saw_is_never_shown_as_working() {
        // Unreachable for a row the evaluator has seen, since a prompt is not
        // dismissible. It is the fallback for a row built without one, and the
        // reading that does not hide a request from the person it waits on.
        assert_eq!(
            section_of(&row_without_attention("a", AgentStatus::NeedsApproval("x".into()), false)),
            SectionKind::Attention
        );
    }

    #[test]
    fn the_sections_are_in_the_order_the_phase_fixed() {
        let labels: Vec<&str> = SectionKind::ORDER.iter().map(|k| k.label()).collect();
        assert_eq!(
            labels,
            vec!["NEEDS YOU", "FINISHED · UNREAD", "WORKING", "DONE", "IDLE", "ENDED"]
        );
        // The sort key and the bucket index are one number, so a sorted list
        // cannot come out shuffled inside its own headers.
        for (want, kind) in SectionKind::ORDER.iter().enumerate() {
            assert_eq!(kind.index(), want, "{} is out of place", kind.label());
        }
    }

    #[test]
    fn idle_with_reply_is_an_unread_finish_until_somebody_reads_it() {
        // It used to land in DONE the moment the turn ended, beside
        // finishes from last week. A reply nobody has read is what this
        // phase added a section for, and DONE is the read-finished bucket.
        assert_eq!(
            section_of(&row("a", AgentStatus::Idle, true)),
            SectionKind::FinishedUnread
        );
        assert_eq!(
            section_of(&row_without_attention("a", AgentStatus::Idle, true)),
            SectionKind::Done,
            "read, so it drops to DONE"
        );
    }

    #[test]
    fn group_emits_headers_in_severity_order_with_counts() {
        let rows = vec![
            row("done1", AgentStatus::Done { code: Some(0) }, false),
            row("run1", AgentStatus::Running, false),
            row("done2", AgentStatus::Idle, true), // replied, unread
            row("wait1", AgentStatus::WaitingForInput, false),
        ];
        let items = group_into_sections(rows);
        // The clean exit and the idle-with-reply row are both unread
        // finishes now, so they share a section instead of DONE.
        let mut headers = items.iter().filter_map(|i| match i {
            DashboardItem::Header { kind, count } => Some((*kind, *count)),
            DashboardItem::Row(_) => None,
        });
        assert_eq!(headers.next(), Some((SectionKind::Attention, 1)));
        assert_eq!(headers.next(), Some((SectionKind::FinishedUnread, 2)));
        assert_eq!(headers.next(), Some((SectionKind::Working, 1)));
        assert_eq!(headers.next(), None);
    }

    #[test]
    fn group_drops_empty_sections() {
        let rows = vec![row("a", AgentStatus::Idle, false)];
        let items = group_into_sections(rows);
        // Only the Idle section: one header + one row.
        assert_eq!(items.len(), 2);
        match &items[0] {
            DashboardItem::Header { kind, count } => {
                assert_eq!(*kind, SectionKind::Idle);
                assert_eq!(*count, 1);
            }
            DashboardItem::Row(_) => panic!("first item must be the header"),
        }
        assert!(matches!(items[1], DashboardItem::Row(_)));
    }

    #[test]
    fn group_preserves_row_order_within_a_section() {
        let rows = vec![
            row("first", AgentStatus::Running, false),
            row("second", AgentStatus::Running, false),
        ];
        let items = group_into_sections(rows);
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                DashboardItem::Row(r) => Some(r.db_id.as_str()),
                DashboardItem::Header { .. } => None,
            })
            .collect();
        assert_eq!(ids, vec!["first", "second"]);
    }

    #[test]
    fn group_empty_is_empty() {
        assert!(group_into_sections(vec![]).is_empty());
    }
}
