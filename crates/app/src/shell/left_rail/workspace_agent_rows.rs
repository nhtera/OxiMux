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
    is_active: bool,
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
                is_active,
                weak_root.clone(),
                theme,
                density,
                typography,
            ));
        }
    }
    col
}

/// The agent's title — its most recent prompt, the one field that distinguishes
/// otherwise-identical idle rows. Prefers the LIVE prompt from the status
/// channel (cached across the turn by the poll loop, so it survives tool steps
/// and idle); falls back to the DB-persisted title so a restored or re-adopted
/// session keeps its title across an app restart instead of decaying to the
/// status verb. `None` only when neither source has a prompt. Not truncated
/// here — the caller composes it with the activity before a single truncation.
fn live_prompt(row: &RailAgentRow) -> Option<String> {
    let live = row
        .status_rx
        .as_ref()
        .and_then(|rx| rx.borrow().detail.as_ref().and_then(|d| d.prompt.clone()));
    let chosen = live.or_else(|| row.persisted_title.clone())?;
    let trimmed = chosen.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

/// One agent row's composed live title: the user's prompt (the agent's title)
/// optionally trailed by its current activity (`"add tests · Edit: foo.rs"`),
/// or just the prompt, or just the activity. Whitespace is collapsed to a
/// single line so a multi-line prompt never breaks the row. `None` when the row
/// has neither — a history row, or a live agent not yet prompted since launch.
///
/// Shared so the single-agent workspace card and the multi-agent disclosure
/// sub-row render the same title from the same live source.
pub struct LiveTitle {
    pub text: String,
    /// `true` when a captured prompt leads the text, so callers can render it
    /// as primary; activity-only text reads as secondary.
    pub leads_with_prompt: bool,
}

/// Compose [`LiveTitle`] from a row's live status receiver. See the type docs.
pub fn live_title(row: &RailAgentRow) -> Option<LiveTitle> {
    let norm = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match (live_prompt(row), live_activity(row)) {
        (Some(prompt), Some(activity)) => Some(LiveTitle {
            text: norm(format!("{prompt} · {activity}")),
            leads_with_prompt: true,
        }),
        (Some(prompt), None) => Some(LiveTitle {
            text: norm(prompt),
            leads_with_prompt: true,
        }),
        (None, Some(activity)) => Some(LiveTitle {
            text: norm(activity),
            leads_with_prompt: false,
        }),
        (None, None) => None,
    }
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
///
/// `is_active` (the workspace is the selected one) expands the row: the
/// descriptor WRAPS to its full text on multiple lines and the row grows,
/// instead of clipping to a single ellipsised line. Inactive workspaces keep
/// the compact one-line rows so the rail stays scannable.
fn render_agent_sub_row(
    row: &RailAgentRow,
    is_active: bool,
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
    // status.
    let (descriptor, descriptor_color) = match live_title(row) {
        Some(t) => (
            t.text,
            if t.leads_with_prompt {
                theme.fg_base
            } else {
                theme.fg_muted
            },
        ),
        None => (v.verb.to_string(), v.verb_color),
    };
    // Active workspace → wrap the full descriptor; inactive → one ellipsised
    // line. The dot/icon/age align to the top when wrapping so they sit with
    // the first line of text.
    let descriptor_elem = if is_active {
        div().flex_1().min_w_0().child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(descriptor_color)
                .child(descriptor),
        )
    } else {
        // Keep the descriptor in a flex-row so a truncating line never collapses
        // to blank (a known flex-col text pitfall). `.truncate()` clips it to one
        // line with an ellipsis at the row width.
        div().flex_1().min_w_0().flex().flex_row().items_center().child(
            div()
                .min_w_0()
                .text_size(px(typography.t_body_sm))
                .text_color(descriptor_color)
                .truncate()
                .child(truncate_chars(&descriptor, 48)),
        )
    };
    let mut base = div()
        .flex()
        .flex_row()
        .w_full()
        .pl(px(density.pad_panel * 3.0))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline));
    base = if is_active {
        base.items_start()
            .min_h(px(density.h_row))
            .py(px(density.gap_inline))
    } else {
        base.items_center().h(px(density.h_row))
    };
    let base = base
        .child(
            div()
                .size(px(SUB_DOT))
                .rounded_full()
                .bg(v.dot)
                .flex_shrink_0()
                .when(is_active, |d| d.mt(px(5.0))),
        )
        .child(
            svg()
                .path(adapter_icon_path(&row.adapter_id))
                .size(px(13.0))
                .text_color(theme.fg_muted)
                .flex_shrink_0()
                .when(is_active, |d| d.mt(px(2.0))),
        )
        .child(descriptor_elem)
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
            persisted_title: None,
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

    /// Build a live row whose snapshot carries the given detail. The sender is
    /// dropped here; a `watch` receiver still reads the last value after that.
    fn row_with_detail(detail: oximux_core::SidebandDetail) -> RailAgentRow {
        let (tx, rx) = live_rx(AgentStatus::Running);
        tx.send(AgentSnapshot {
            status: AgentStatus::Running,
            detail: Some(detail),
        })
        .unwrap();
        row(true, AgentStatus::Idle, Some(rx))
    }

    #[test]
    fn live_title_leads_with_prompt() {
        let detail = oximux_core::SidebandDetail {
            prompt: Some("add a readme section".into()),
            tool_name: Some("Edit".into()),
            tool_input_summary: Some("README.md".into()),
            ..Default::default()
        };
        let t = live_title(&row_with_detail(detail)).expect("prompt yields a title");
        assert_eq!(t.text, "add a readme section · Edit: README.md");
        assert!(t.leads_with_prompt);
    }

    #[test]
    fn live_title_prompt_only() {
        let detail = oximux_core::SidebandDetail {
            prompt: Some("  hi  ".into()),
            ..Default::default()
        };
        let t = live_title(&row_with_detail(detail)).expect("prompt yields a title");
        assert_eq!(t.text, "hi");
        assert!(t.leads_with_prompt);
    }

    #[test]
    fn live_title_activity_only_is_not_a_prompt() {
        let detail = oximux_core::SidebandDetail {
            tool_name: Some("Bash".into()),
            tool_input_summary: Some("cargo test".into()),
            ..Default::default()
        };
        let t = live_title(&row_with_detail(detail)).expect("activity yields a title");
        assert_eq!(t.text, "Bash: cargo test");
        assert!(!t.leads_with_prompt);
    }

    #[test]
    fn live_title_none_for_history_row() {
        // No status receiver and no persisted title (history) → no live title.
        assert!(live_title(&row(false, AgentStatus::Idle, None)).is_none());
    }

    #[test]
    fn live_title_falls_back_to_persisted_title() {
        // A live row whose channel carries no prompt (e.g. just re-adopted)
        // still shows its title from the persisted column.
        let (_tx, rx) = live_rx(AgentStatus::Idle);
        let mut r = row(true, AgentStatus::Idle, Some(rx));
        r.persisted_title = Some("  resume the migration  ".into());
        let t = live_title(&r).expect("persisted title yields a title");
        assert_eq!(t.text, "resume the migration");
        assert!(t.leads_with_prompt);
    }

    #[test]
    fn live_prompt_prefers_live_over_persisted() {
        // When both exist, the live (current-turn) prompt wins over the stale
        // persisted one.
        let detail = oximux_core::SidebandDetail {
            prompt: Some("live prompt".into()),
            ..Default::default()
        };
        let mut r = row_with_detail(detail);
        r.persisted_title = Some("old persisted".into());
        let t = live_title(&r).expect("title");
        assert_eq!(t.text, "live prompt");
    }

    #[test]
    fn persisted_title_shows_on_a_non_live_history_row() {
        // A finished row with a persisted title keeps showing it.
        let mut r = row(false, AgentStatus::Done { code: Some(0) }, None);
        r.persisted_title = Some("ship the release".into());
        assert_eq!(
            live_title(&r).map(|t| t.text),
            Some("ship the release".to_string())
        );
    }

    #[test]
    fn live_title_collapses_multiline_prompt() {
        let detail = oximux_core::SidebandDetail {
            prompt: Some("fix the bug\n\nin the parser".into()),
            ..Default::default()
        };
        let t = live_title(&row_with_detail(detail)).expect("prompt yields a title");
        assert_eq!(t.text, "fix the bug in the parser");
    }
}
