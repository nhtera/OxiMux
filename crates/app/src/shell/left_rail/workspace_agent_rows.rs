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
use gpui::{Entity, Hsla, MouseButton, WeakEntity, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agent_presentation::{adapter_icon_path, agent_verb};
use crate::shell::left_rail::{LeftRail, RailAgentRow};
use crate::workspace_root::WorkspaceRoot;

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
        verb: v.label,
        verb_color: v.color,
    }
}

/// Render the disclosure for one workspace: a clickable summary line plus,
/// when expanded, a sub-row per agent. Caller guarantees `rows.len() > 1`.
#[allow(clippy::too_many_arguments)]
pub fn render_workspace_agent_disclosure(
    workspace_key: &str,
    rows: &[RailAgentRow],
    is_expanded: bool,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
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
            col = col.child(render_agent_sub_row(
                row,
                weak_root.clone(),
                theme,
                density,
                typography,
            ));
        }
    }
    col
}

/// The user's prompt for this agent, read live from its status receiver. This
/// is the agent's title — the one field that distinguishes otherwise-identical
/// idle rows. Cached across the turn by the poll loop, so it survives tool
/// steps and idle. `None` for history rows or an agent that hasn't been
/// prompted since launch (e.g. a restored session). Not truncated here — the
/// caller composes it with the activity before a single truncation.
fn live_prompt(row: &RailAgentRow) -> Option<String> {
    let snap = row.status_rx.as_ref()?.borrow();
    let prompt = snap.detail.as_ref()?.prompt.as_deref()?;
    let prompt = prompt.trim();
    (!prompt.is_empty()).then(|| prompt.to_string())
}

/// Live tool/message line read straight from the agent's status receiver:
/// the tool it is invoking (`"Edit: foo.rs"`) or its last free-form message.
/// `None` for history rows or a live agent with no current detail — the row
/// then falls back to its prompt or status verb. Not truncated here.
fn live_activity(row: &RailAgentRow) -> Option<String> {
    let snap = row.status_rx.as_ref()?.borrow();
    let detail = snap.detail.as_ref()?;
    let text = if let Some(tool) = detail.tool_name.as_deref().filter(|t| !t.is_empty()) {
        match detail.tool_input_summary.as_deref().filter(|s| !s.is_empty()) {
            Some(input) => format!("{tool}: {input}"),
            None => tool.to_string(),
        }
    } else {
        detail.last_message.as_deref().filter(|m| !m.is_empty())?.to_string()
    };
    Some(text)
}

/// Single-line truncation with an ellipsis, counting characters (not bytes)
/// so multi-byte content never splits a codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One expanded agent sub-row, mirroring the reference cockpit's compact row:
/// `[status dot] [adapter icon] [activity / verb]  …  [relative age]`. A live
/// row is clickable to focus its tab; a history-only row is display-only.
fn render_agent_sub_row(
    row: &RailAgentRow,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let v = sub_row_view(row, theme);
    // Title precedence, mirroring the reference cockpit's compact row:
    //   prompt (the agent's title) · live tool/message
    //   prompt
    //   live tool/message (no prompt captured yet)
    //   status verb (a row with no live detail at all)
    // The prompt leads and reads as primary text; the dot color carries the
    // status. Composed first, then truncated as one unit so the title (the
    // priority) survives when the trailing activity overflows.
    let (descriptor, descriptor_color) = match (live_prompt(row), live_activity(row)) {
        (Some(prompt), Some(activity)) => (format!("{prompt} · {activity}"), theme.fg_base),
        (Some(prompt), None) => (prompt, theme.fg_base),
        (None, Some(activity)) => (activity, theme.fg_muted),
        (None, None) => (v.verb.to_string(), v.verb_color),
    };
    let descriptor = truncate_chars(&descriptor, 48);
    let base = div()
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
            svg()
                .path(adapter_icon_path(&row.adapter_id))
                .size(px(13.0))
                .text_color(theme.fg_muted)
                .flex_shrink_0(),
        )
        .child(
            // Keep the descriptor in a flex-row so a truncating line never
            // collapses to blank (a known flex-col text pitfall).
            div().flex_1().min_w_0().flex().flex_row().items_center().child(
                div()
                    .min_w_0()
                    .text_size(px(typography.t_body_sm))
                    .text_color(descriptor_color)
                    .child(descriptor),
            ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(row.age_label.clone()),
        );
    if row.is_live {
        let db_id = row.db_id.clone();
        base.cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak_root
                    .update(cx, |root, cx| root.focus_agent_by_db_id(&db_id, window, cx));
            })
    } else {
        base
    }
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
            age_label: "3d".into(),
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
