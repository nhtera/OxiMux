//! Status-section grouping for the agents dashboard — the `DONE 4`-style
//! headers from the reference cockpit. Pure + testable: turns an
//! attention-sorted `Vec<AgentRow>` into a flat `Header | Row` item list,
//! severity-ordered, empty sections dropped. The render layer (mod.rs) walks
//! the items and paints a header or a card per entry.

use oximux_core::AgentStatus;

use crate::shell::agents_dashboard::model::AgentRow;

/// A status section, in severity order (top of the list first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionKind {
    /// Blocked on the user — approval or input required.
    Attention,
    /// Actively running.
    Working,
    /// Finished a turn / clean exit.
    Done,
    /// Ready, never run a turn.
    Idle,
    /// Interrupted / failed / non-zero exit.
    Ended,
}

impl SectionKind {
    /// Severity order — drives both the bucket index and the render order.
    pub const ORDER: [SectionKind; 5] = [
        SectionKind::Attention,
        SectionKind::Working,
        SectionKind::Done,
        SectionKind::Idle,
        SectionKind::Ended,
    ];

    /// Position in [`SectionKind::ORDER`] (the bucket index).
    fn index(self) -> usize {
        match self {
            SectionKind::Attention => 0,
            SectionKind::Working => 1,
            SectionKind::Done => 2,
            SectionKind::Idle => 3,
            SectionKind::Ended => 4,
        }
    }

    /// Small-caps header label.
    pub fn label(self) -> &'static str {
        match self {
            SectionKind::Attention => "NEEDS ATTENTION",
            SectionKind::Working => "WORKING",
            SectionKind::Done => "DONE",
            SectionKind::Idle => "IDLE",
            SectionKind::Ended => "ENDED",
        }
    }
}

/// Which section a row belongs to. A finished-turn row (idle + reply) is "done"
/// even though its raw status is Idle, matching the card's emerald check.
pub fn section_of(row: &AgentRow) -> SectionKind {
    if row.completed_turn {
        return SectionKind::Done;
    }
    match row.status.as_ref() {
        Some(AgentStatus::NeedsApproval(_)) | Some(AgentStatus::WaitingForInput) => {
            SectionKind::Attention
        }
        Some(AgentStatus::Running) => SectionKind::Working,
        Some(AgentStatus::Done { code: Some(0) }) => SectionKind::Done,
        Some(AgentStatus::Idle) | None => SectionKind::Idle,
        Some(AgentStatus::Done { .. })
        | Some(AgentStatus::Failed(_))
        | Some(AgentStatus::Interrupted) => SectionKind::Ended,
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
    let mut buckets: [Vec<AgentRow>; 5] = std::array::from_fn(|_| Vec::new());
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

    fn row(id: &str, status: AgentStatus, completed_turn: bool) -> AgentRow {
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
        }
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
            section_of(&row("d", AgentStatus::Done { code: Some(0) }, false)),
            SectionKind::Done
        );
        assert_eq!(
            section_of(&row("e", AgentStatus::Idle, false)),
            SectionKind::Idle
        );
        assert_eq!(
            section_of(&row("f", AgentStatus::Failed("x".into()), false)),
            SectionKind::Ended
        );
        assert_eq!(
            section_of(&row("g", AgentStatus::Interrupted, false)),
            SectionKind::Ended
        );
    }

    #[test]
    fn idle_with_reply_is_done() {
        // A finished-turn row reads as Done even though its raw status is Idle.
        assert_eq!(
            section_of(&row("a", AgentStatus::Idle, true)),
            SectionKind::Done
        );
    }

    #[test]
    fn group_emits_headers_in_severity_order_with_counts() {
        let rows = vec![
            row("done1", AgentStatus::Done { code: Some(0) }, false),
            row("run1", AgentStatus::Running, false),
            row("done2", AgentStatus::Idle, true), // finished turn → done
            row("wait1", AgentStatus::WaitingForInput, false),
        ];
        let items = group_into_sections(rows);
        // Order: Attention header, run? no — Attention(1), Working(1), Done(2).
        let mut headers = items.iter().filter_map(|i| match i {
            DashboardItem::Header { kind, count } => Some((*kind, *count)),
            DashboardItem::Row(_) => None,
        });
        assert_eq!(headers.next(), Some((SectionKind::Attention, 1)));
        assert_eq!(headers.next(), Some((SectionKind::Working, 1)));
        assert_eq!(headers.next(), Some((SectionKind::Done, 2)));
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
