//! Single-card painter for the agents dashboard.
//!
//! Each card is a per-agent-session row mirroring the reference cockpit's
//! agent card:
//!
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ (✓) ✳  add tests · Edit: foo.rs              10 hours ago │  line 1
//!   │        All set — the suite is green.                      │  line 2
//!   │        [oximux]  main                                     │  line 3
//!   └──────────────────────────────────────────────────────────┘
//!
//! The state glyph (emerald check / stepped spinner / colored dot) and the
//! prompt/activity text reuse the rail's shared helpers so a card reads exactly
//! like a rail sub-row. Action-required rows (tier 0: NeedsApproval /
//! WaitingForInput) get floating-card chrome plus a 2px left accent bar so they
//! read as "come look at me".

use gpui::{Div, InteractiveElement, ParentElement, Styled, div, px, svg};
use oximux_core::AgentStatus;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::model::AgentRow;
use crate::shell::agents_dashboard::sections::SectionKind;
use crate::shell::left_rail::workspace_agent_rows::{agent_state_indicator, agent_state_label};
use crate::ui::overlay::FloatingSurface;

/// Fixed card height — fits the 3-line card (title + secondary + badges) with
/// breathing room.
pub(crate) const ROW_HEIGHT: f32 = 64.0;

/// Height of a status-section header row.
const SECTION_HEADER_HEIGHT: f32 = 26.0;

/// A status-section header — the small-caps section name + a count, like the
/// reference cockpit's `DONE 4`. Sits above its cards in the scroll column.
pub(crate) fn render_section_header(
    kind: SectionKind,
    count: usize,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(SECTION_HEADER_HEIGHT))
        .px(px(density.pad_panel))
        .pt(px(density.gap_inline))
        .gap(px(density.gap_inline))
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(kind.label()),
        )
        .child(
            // Count as a subtle rounded badge, like the reference cockpit.
            div()
                .flex_shrink_0()
                .px(px(5.))
                .rounded(px(density.r_xs))
                .bg(theme.hover_overlay)
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(count.to_string()),
        )
}

/// The card's second line: the live tool step or reply, else what the row is
/// asking for, else its state label.
///
/// The evaluator's answer comes before the status label because the two can
/// disagree and the status is the weaker word. A chat row's display status is
/// `Idle` whenever it is not working, so a failed chat — and a chat with no
/// tab open to read a status from at all — said "Idle" underneath a NEEDS YOU
/// header. The live line still wins: it is the newest thing the agent did,
/// and for a blocked tracked agent it is the request itself.
///
/// The indicator glyph still reads the status alone. It is shared with the
/// rail and takes an `AgentStatus`; PR D replaces both with the attention's
/// own verb and tone rather than mapping backwards from one to the other.
fn state_line(row: &AgentRow, status: &AgentStatus) -> String {
    row.secondary
        .clone()
        .or_else(|| row.attention.as_ref().and_then(|a| a.reason.clone()))
        .unwrap_or_else(|| agent_state_label(status).to_string())
}

/// Size of the adapter brand glyph beside the state indicator.
const ICON_GLYPH_SIZE: f32 = 14.0;

/// Paint one `AgentRow` into a GPUI element. Height is `ROW_HEIGHT` so all
/// cards share one height for `uniform_list`.
pub fn render_agent_row(
    row: &AgentRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    let status = row.status.clone().unwrap_or(AgentStatus::Idle);
    // The evaluator's answer, not a status guess: a failure that has been
    // viewed no longer wears the accent, and a stall now does.
    let is_attention =
        row.attention.as_ref().is_some_and(|attention| attention.kind.is_needs_you());

    // ── leading column: state glyph + adapter icon, sat on the title line ──
    let lead = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(20.))
        .gap(px(density.gap_inline))
        .child(agent_state_indicator(
            &status,
            row.completed_turn,
            &row.db_id,
            theme,
        ))
        .child(
            svg()
                .path(row.icon_path)
                .size(px(ICON_GLYPH_SIZE))
                .text_color(theme.fg_muted)
                .flex_shrink_0(),
        );

    // ── line 1: prompt title (else adapter label) + relative age, right ──
    let primary = row.primary.as_deref().unwrap_or(row.label.as_str());
    // A captured prompt leads brighter; the adapter-name fallback reads dimmer.
    let primary_color = if row.primary.is_some() {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(density.gap_inline))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(typography.t_body_sm))
                .text_color(primary_color)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(primary.to_string()),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(row.age_label.clone()),
        );

    let secondary = state_line(row, &status);
    let line2 = (!secondary.is_empty()).then(|| {
        div()
            .w_full()
            .min_w_0()
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(secondary)
    });

    // ── line 3: repo pill + branch chip ──
    let branch_or_slug = if row.workspace.branch.is_empty() {
        row.workspace.slug.as_str()
    } else {
        row.workspace.branch.as_str()
    };
    let line3 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .flex_shrink_0()
                .px(px(4.))
                .rounded(px(density.r_xs))
                .bg(theme.hover_overlay)
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .whitespace_nowrap()
                .child(row.project_name.clone()),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(branch_or_slug.to_string()),
        );

    let text_col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(2.))
        .child(line1)
        .children(line2)
        .child(line3);

    let root = div()
        .flex()
        .flex_row()
        .items_start()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(density.pad_panel))
        .py(px(density.gap_inline))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .child(lead)
        .child(text_col);

    if is_attention {
        // Floating-card chrome (relative + border + rounded + top highlight)
        // plus a 2px left accent bar in the warn color. The bar is absolute,
        // anchored to the `relative` context floating_chrome establishes.
        root.floating_chrome(&theme, &density).child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(2.))
                .bg(theme.status_warn),
        )
    } else {
        root.hover(|s| s.bg(theme.hover_overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::agents_dashboard::model::AgentRow;
    use crate::shell::left_rail::RailAgentTarget;
    use oximux_agents::attention::{Attention, AttentionKind, AttentionSource};

    fn row(secondary: Option<&str>, attention: Option<Attention>) -> AgentRow {
        AgentRow {
            project_name: "oximux".into(),
            workspace: oximux_core::Workspace {
                id: "ws".into(),
                project_id: "p".into(),
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
            },
            db_id: "s1".into(),
            target: RailAgentTarget::Chat { session_id: "s1".into() },
            age_label: "now".into(),
            status: Some(AgentStatus::Idle),
            is_live: true,
            icon_path: "icons/sparkles.svg",
            last_active_at: None,
            label: "Chat".into(),
            primary: Some("Fix the parser".into()),
            secondary: secondary.map(str::to_string),
            completed_turn: false,
            attention,
        }
    }

    fn attention(kind: AttentionKind, reason: &str) -> Attention {
        Attention {
            kind,
            since: 0,
            reason: Some(reason.to_string()),
            source: AttentionSource::External("chat:s1".into()),
        }
    }

    #[test]
    fn a_blocked_row_says_what_it_wants_rather_than_its_status() {
        // A chat's display status is `Idle` whenever it is not working, so the
        // status label read "Idle" on a card sitting under NEEDS YOU.
        let blocked = row(None, Some(attention(AttentionKind::Failed, "spawn failed: no binary")));
        assert_eq!(state_line(&blocked, &AgentStatus::Idle), "spawn failed: no binary");
    }

    #[test]
    fn the_live_line_still_wins_because_it_is_the_newest_thing_that_happened() {
        let working = row(
            Some("Edit: parser.rs"),
            Some(attention(AttentionKind::NeedsPermission, "rm -rf build/")),
        );
        assert_eq!(state_line(&working, &AgentStatus::Running), "Edit: parser.rs");
    }

    #[test]
    fn a_row_with_nothing_to_say_falls_back_to_its_state_label() {
        assert_eq!(state_line(&row(None, None), &AgentStatus::Running), "Working");
    }
}
