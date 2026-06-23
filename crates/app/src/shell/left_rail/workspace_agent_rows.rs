//! The "N agents" disclosure rendered under a workspace row.
//!
//! A workspace with more than one agent gets a compact, clickable summary
//! line (a chevron, the count, and one status glyph per agent) that expands
//! into a slim status sub-row per agent. Single-agent workspaces keep the
//! existing single dot on the row itself and render nothing here.
//!
//! The collapsed cluster is the headline visual — it renders with no
//! interaction. Sub-rows are display-only in this slice; per-row focus is a
//! follow-up. Status colors + verbs reuse `agent_presentation::agent_verb`,
//! so the disclosure matches the tab badge and dashboard.

use gpui::prelude::*;
use gpui::{Entity, Hsla, MouseButton, SharedString, div, px};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agent_presentation::agent_verb;
use crate::shell::left_rail::{LeftRail, RailAgentRow};

/// Max status glyphs shown in the collapsed cluster before an "+N" overflow.
pub const MAX_GLYPHS: usize = 5;

const GLYPH_DOT: f32 = 6.0;
const SUB_DOT: f32 = 7.0;

/// Collapsed-cluster dot colors — one per agent, capped at [`MAX_GLYPHS`].
/// Live agents reflect their current status; history rows use the DB status.
pub fn glyph_colors(rows: &[RailAgentRow], theme: Theme) -> Vec<Hsla> {
    rows.iter()
        .take(MAX_GLYPHS)
        .map(|r| {
            let status = r.effective_status();
            agent_verb(Some(&status), r.is_live, theme).color
        })
        .collect()
}

/// Agents beyond the glyph cap, surfaced as a trailing "+N".
pub fn glyph_overflow(count: usize) -> usize {
    count.saturating_sub(MAX_GLYPHS)
}

/// Display fields for one expanded sub-row.
pub struct SubRowView {
    pub dot: Hsla,
    pub label: SharedString,
    pub verb: &'static str,
    pub verb_color: Hsla,
}

/// Resolve one agent's sub-row view. The effective status (live snapshot when
/// live, else the DB status) drives both the dot color and the verb.
pub fn sub_row_view(row: &RailAgentRow, theme: Theme) -> SubRowView {
    let status = row.effective_status();
    let v = agent_verb(Some(&status), row.is_live, theme);
    SubRowView {
        dot: v.color,
        label: row.label.clone(),
        verb: v.label,
        verb_color: v.color,
    }
}

/// Render the disclosure for one workspace: a clickable summary line plus,
/// when expanded, a sub-row per agent. Caller guarantees `rows.len() > 1`.
pub fn render_workspace_agent_disclosure(
    workspace_key: &str,
    rows: &[RailAgentRow],
    is_expanded: bool,
    rail: Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let ws = workspace_key.to_string();
    let count = rows.len();
    let overflow = glyph_overflow(count);
    let chevron = if is_expanded { "▾" } else { "▸" };

    let mut cluster = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(format!("{chevron} {count}")),
        );
    for color in glyph_colors(rows, theme) {
        cluster = cluster.child(
            div()
                .size(px(GLYPH_DOT))
                .rounded_full()
                .bg(color)
                .flex_shrink_0(),
        );
    }
    if overflow > 0 {
        cluster = cluster.child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(format!("+{overflow}")),
        );
    }

    let summary = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        // Indent under the workspace card's dot + label slot.
        .pl(px(density.pad_panel * 2.0))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
            rail.update(cx, |r, cx| {
                r.toggle_workspace_expanded(&ws);
                cx.notify();
            });
        })
        .child(cluster);

    let mut col = div().flex().flex_col().w_full().child(summary);
    if is_expanded {
        for row in rows {
            col = col.child(render_agent_sub_row(row, theme, density, typography));
        }
    }
    col
}

/// One expanded agent sub-row: status dot + label + status verb, indented
/// deeper than the summary line. Display-only for now.
fn render_agent_sub_row(
    row: &RailAgentRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let v = sub_row_view(row, theme);
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        .pl(px(density.pad_panel * 3.0))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(
            div()
                .size(px(SUB_DOT))
                .rounded_full()
                .bg(v.dot)
                .flex_shrink_0(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(v.label),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(v.verb_color)
                .child(v.verb),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::AgentStatusStream;
    use oximux_core::{AgentSnapshot, AgentStatus};
    use tokio::sync::watch;

    fn live_rx(status: AgentStatus) -> (watch::Sender<AgentSnapshot>, AgentStatusStream) {
        watch::channel(AgentSnapshot::from_status(status))
    }

    fn row(is_live: bool, db_status: AgentStatus, rx: Option<AgentStatusStream>) -> RailAgentRow {
        RailAgentRow {
            db_id: "id".into(),
            workspace_key: "ws".into(),
            adapter_id: "claude-code".into(),
            label: "Claude Code".into(),
            is_live,
            status_rx: rx,
            db_status,
            started_at: Some("2026-06-23T10:00:00Z".into()),
            ended_at: None,
        }
    }

    #[test]
    fn glyph_colors_caps_at_max() {
        let rows: Vec<RailAgentRow> = (0..8)
            .map(|_| row(false, AgentStatus::Idle, None))
            .collect();
        assert_eq!(glyph_colors(&rows, Theme::default()).len(), MAX_GLYPHS);
    }

    #[test]
    fn overflow_math() {
        assert_eq!(glyph_overflow(8), 3);
        assert_eq!(glyph_overflow(5), 0);
        assert_eq!(glyph_overflow(1), 0);
    }

    #[test]
    fn sub_row_uses_live_status_over_db() {
        // DB says Idle, but the live receiver says Running — the sub-row verb
        // must reflect the live (effective) status.
        let theme = Theme::default();
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let live = row(true, AgentStatus::Idle, Some(rx));
        let running_verb = agent_verb(Some(&AgentStatus::Running), true, theme).label;
        assert_eq!(sub_row_view(&live, theme).verb, running_verb);
    }

    #[test]
    fn sub_row_history_uses_db_status() {
        let theme = Theme::default();
        let hist = row(false, AgentStatus::Done { code: Some(0) }, None);
        let done_verb = agent_verb(Some(&AgentStatus::Done { code: Some(0) }), false, theme).label;
        assert_eq!(sub_row_view(&hist, theme).verb, done_verb);
    }
}
