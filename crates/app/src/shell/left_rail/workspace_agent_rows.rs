//! The "N agents" disclosure rendered under a workspace row.
//!
//! A workspace with more than one agent gets a compact, clickable summary
//! line — a grouped status cluster (one `[dot] count` chip per indicator
//! color), the agent count, and a chevron — that expands into a slim status
//! sub-row per agent. Single-agent workspaces keep the existing single dot on
//! the row itself and render nothing here.
//!
//! The collapsed cluster is the headline visual — it renders with no
//! interaction. Live sub-rows focus their backing agent or ambient terminal.
//! Status colors + verbs reuse `agent_presentation::agent_verb`, so the
//! disclosure matches the tab badge and dashboard.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, Animation, AnimationExt as _, Entity, Hsla, MouseButton, SharedString,
    Transformation, WeakEntity, div, percentage, px, svg,
};
use gpui_component::Icon;
use oximux_core::AgentStatus;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agent_presentation::{adapter_icon_path, agent_verb};
use crate::shell::left_rail::{LeftRail, RailAgentRow, RailAgentTarget};
use crate::workspace_root::WorkspaceRoot;

/// Reference-cockpit state-indicator metrics: a 10px box holding a 6px dot, a
/// 10px rotating spinner glyph (Working), or an 11px circle-check glyph (Done).
const STATE_BOX: f32 = 10.0;
const STATE_DOT: f32 = 6.0;
const STATE_SPINNER: f32 = 10.0;
const STATE_CHECK: f32 = 11.0;

/// Discrete steps per rotation for the Working spinner. Mirrors the reference
/// cockpit's `steps(12)` cadence: the glyph ticks in 12 mechanical jumps per
/// second rather than a smooth sweep, which reads as "agent is working" without
/// the optical buzz of a continuous spin.
const SPINNER_STEPS: f32 = 12.0;

/// One agent's state indicator, mirroring the reference cockpit's `AgentStateDot`:
/// Done / finished-a-turn → a green circle-check, Working → a rotating spinner,
/// everything else → a colored dot (idle grey, needs-approval amber, attention
/// red).
///
/// `completed_turn` marks an idle agent that has produced a reply (the reference
/// cockpit's "done") so it renders the emerald check rather than the grey idle
/// dot a never-run agent shows. `row_key` seeds the spinner's animation id so
/// concurrently-running agents each keep their own rotation phase.
pub fn agent_state_indicator(
    status: &AgentStatus,
    completed_turn: bool,
    row_key: &str,
    theme: Theme,
) -> AnyElement {
    let box_el = || {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(STATE_BOX))
            .flex_shrink_0()
    };
    let dot = |color: Hsla| div().size(px(STATE_DOT)).rounded_full().bg(color);
    let done_check = || {
        box_el()
            .child(
                Icon::default()
                    .path("icons/circle-check.svg")
                    .size(px(STATE_CHECK))
                    .text_color(theme.status_ok),
            )
            .into_any_element()
    };
    match status {
        // A cleanly-exited process OR an alive agent that finished its turn both
        // read as "done" — the reference cockpit's emerald check.
        AgentStatus::Done { code: Some(0) } => done_check(),
        AgentStatus::Idle if completed_turn => done_check(),
        AgentStatus::Running => box_el()
            .child(
                Icon::default()
                    .path("icons/loader-circle.svg")
                    .size(px(STATE_SPINNER))
                    .text_color(theme.status_warn)
                    .with_animation(
                        SharedString::from(format!("agent-spinner-{row_key}")),
                        Animation::new(Duration::from_secs(1)).repeat(),
                        |icon, delta| {
                            // Quantize to SPINNER_STEPS discrete positions so the
                            // glyph ticks instead of sweeping.
                            let stepped = (delta * SPINNER_STEPS).floor() / SPINNER_STEPS;
                            icon.transform(Transformation::rotate(percentage(stepped)))
                        },
                    ),
            )
            .into_any_element(),
        AgentStatus::Idle => box_el().child(dot(theme.status_muted)).into_any_element(),
        AgentStatus::NeedsApproval(_) => box_el().child(dot(theme.status_warn)).into_any_element(),
        // WaitingForInput / Interrupted / Failed / Done(non-zero) → attention red.
        _ => box_el().child(dot(theme.status_error)).into_any_element(),
    }
}

/// State label for the row's secondary text, matching the reference cockpit's
/// vocabulary ("Idle"/"Working"/…) rather than the tab badge's "Ready".
pub fn agent_state_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "Working",
        AgentStatus::Idle => "Idle",
        AgentStatus::WaitingForInput => "Waiting for input",
        AgentStatus::NeedsApproval(_) => "Needs attention",
        AgentStatus::Done { code: Some(0) } => "Done",
        AgentStatus::Done { .. } | AgentStatus::Failed(_) => "Failed",
        AgentStatus::Interrupted => "Interrupted",
    }
}

/// Collapse runs of whitespace (incl. newlines) to single spaces so a multi-line
/// prompt renders as one scannable line.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One collapsed-header status chip: a state-color bucket plus the adapter
/// slugs of the agents in it, so the chip renders as the reference cockpit's
/// pill — a state glyph, the agents' identity icons, and an overflow count.
#[derive(Debug, PartialEq)]
struct CollapsedChip {
    color: Hsla,
    /// The done bucket renders a check glyph instead of a plain dot (like the
    /// expanded rows), so completion stays visually distinct when collapsed.
    is_done: bool,
    /// Adapter slugs of the agents in this bucket, in row order.
    adapters: Vec<String>,
}

/// Collapsed-header status summary: the agents grouped by their indicator color
/// into severity-ordered chips, so the collapsed disclosure shows the state mix
/// at a glance (mirroring the reference cockpit's grouped status cluster)
/// without expanding. Buckets share the per-row indicator's color vocabulary —
/// attention-red first, then working/approval-amber, done-green, idle-grey —
/// and same-color states merge into one chip.
fn collapsed_state_chips(rows: &[RailAgentRow], theme: Theme) -> Vec<CollapsedChip> {
    // [error, warn, ok, muted] — fixed severity order; adapters filled per row.
    let mut buckets: [Vec<String>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for row in rows {
        // A finished-turn agent (idle + has a reply) joins the done bucket so the
        // collapsed cluster matches the expanded rows' emerald check.
        let idx = if is_completed_turn(row) {
            2 // ok (done)
        } else {
            match row.effective_status() {
                AgentStatus::Done { code: Some(0) } => 2, // ok (done)
                AgentStatus::Running | AgentStatus::NeedsApproval(_) => 1, // warn (working/approval)
                AgentStatus::Idle => 3,                   // muted (idle)
                _ => 0, // error (waiting / interrupted / failed / done-nonzero)
            }
        };
        buckets[idx].push(row.adapter_id.clone());
    }
    let meta = [
        (theme.status_error, false),
        (theme.status_warn, false),
        (theme.status_ok, true), // done → check glyph
        (theme.status_muted, false),
    ];
    buckets
        .into_iter()
        .zip(meta)
        .filter(|(adapters, _)| !adapters.is_empty())
        .map(|(adapters, (color, is_done))| CollapsedChip {
            color,
            is_done,
            adapters,
        })
        .collect()
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
    focused_agent: Option<&RailAgentTarget>,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let ws = workspace_key.to_string();
    let count = rows.len();
    let chevron = if is_expanded { "▾" } else { "▸" };

    // The collapsed header shows a grouped status cluster — one pill per
    // indicator color, each carrying a state glyph, the agents' identity icons,
    // and an overflow count — so the state mix reads at a glance. Expanded, the
    // cluster is redundant with the per-agent rows below, so the header swaps to
    // just the count (matching the reference cockpit). The chevron sits right.
    const CHIP_ICONS_MAX: usize = 3;
    const CLUSTER_CHIPS_MAX: usize = 3;
    /// Cluster check glyph — a touch under the row's so it sits beside icons.
    const CHIP_CHECK: f32 = 10.0;

    let left = if is_expanded {
        div()
            .flex_1()
            .min_w_0()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child(format!("{count} agents"))
            .into_any_element()
    } else {
        let chips = collapsed_state_chips(rows, theme);
        // Agents in groups past the visible cap roll into a trailing "+N".
        let hidden_group_agents: usize = chips
            .iter()
            .skip(CLUSTER_CHIPS_MAX)
            .map(|c| c.adapters.len())
            .sum();
        let mut cluster = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w_0()
            .gap(px(density.gap_inline));
        for chip in chips.iter().take(CLUSTER_CHIPS_MAX) {
            let hidden_icons = chip.adapters.len().saturating_sub(CHIP_ICONS_MAX);
            let glyph = if chip.is_done {
                Icon::default()
                    .path("icons/circle-check.svg")
                    .size(px(CHIP_CHECK))
                    .text_color(chip.color)
                    .into_any_element()
            } else {
                div()
                    .size(px(STATE_DOT))
                    .rounded_full()
                    .bg(chip.color)
                    .into_any_element()
            };
            let mut pill = div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(2.0))
                .px(px(3.0))
                .rounded(px(4.0))
                .bg(theme.hover_overlay)
                .child(glyph);
            for adapter in chip.adapters.iter().take(CHIP_ICONS_MAX) {
                pill = pill.child(
                    svg()
                        .path(adapter_icon_path(adapter))
                        .size(px(11.0))
                        .text_color(theme.fg_muted)
                        .flex_shrink_0(),
                );
            }
            if hidden_icons > 0 {
                pill = pill.child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(format!("+{hidden_icons}")),
                );
            }
            cluster = cluster.child(pill);
        }
        if hidden_group_agents > 0 {
            cluster = cluster.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .child(format!("+{hidden_group_agents}")),
            );
        }
        cluster.into_any_element()
    };

    let summary = div()
        // Stable id so GPUI tracks the hover hitbox across re-renders (see the
        // agent row note below) — a bare `.hover()` repaints unreliably.
        .id(SharedString::from(format!("agents-disclosure-{workspace_key}")))
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
        .child(left)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(chevron),
        );

    let mut col = div().flex().flex_col().w_full().child(summary);
    if is_expanded {
        for row in rows {
            // The row the user is currently looking at (its tab is the active
            // pane) stays lit, matching the reference cockpit's focused-pane
            // row. `RailAgentTarget` is the shared identity for both tracked and
            // ambient rows, so one equality check covers both.
            let is_focused = focused_agent == Some(&row.target);
            col = col.child(render_agent_sub_row(
                row,
                is_active,
                is_focused,
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
/// The row's current sideband detail: live from the status channel for a tracked
/// row, or the carried `ambient_detail` for an ambient row (which has no
/// channel). Owned so callers don't hold the `watch` borrow across the render.
fn row_detail(row: &RailAgentRow) -> Option<oximux_core::SidebandDetail> {
    let mut detail = match &row.status_rx {
        Some(rx) => rx.borrow().detail.clone(),
        None => row.ambient_detail.clone(),
    };
    // Restore-time fallback for a tracked row: a re-adopted session's live status
    // channel starts empty (the poll loop is fresh), so surface the DB-persisted
    // last reply until a new turn produces one — the row keeps its finished-turn
    // message + done-check across an app restart instead of reverting to "Idle".
    if let Some(persisted) = row
        .persisted_message
        .as_deref()
        .filter(|m| !m.trim().is_empty())
    {
        let d = detail.get_or_insert_with(Default::default);
        if d.last_message.as_deref().map(str::trim).unwrap_or("").is_empty() {
            d.last_message = Some(persisted.to_string());
        }
    }
    detail
}

/// True when this row's agent FINISHED a turn — the reference cockpit's "done":
/// it is idle but has produced an assistant reply (captured by the `Stop` hook).
/// Drives the emerald done-check, distinguishing a completed agent from one that
/// has never run a turn (which stays a grey idle dot).
pub fn is_completed_turn(row: &RailAgentRow) -> bool {
    matches!(row.effective_status(), AgentStatus::Idle)
        && row_detail(row)
            .and_then(|d| d.last_message)
            .is_some_and(|m| !m.trim().is_empty())
}

pub fn live_prompt(row: &RailAgentRow) -> Option<String> {
    let live = row_detail(row).and_then(|d| d.prompt);
    let chosen = live.or_else(|| row.persisted_title.clone())?;
    let trimmed = chosen.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Live tool/message line read straight from the agent's status receiver:
/// the tool it is invoking (`"Edit: foo.rs"`) or its last free-form message.
/// `None` for history rows or a live agent with no current detail — the row
/// then falls back to its prompt or status verb. Not truncated here.
pub fn live_activity(row: &RailAgentRow) -> Option<String> {
    let detail = row_detail(row)?;
    let text = if let Some(tool) = detail.tool_name.as_deref().filter(|t| !t.is_empty()) {
        match detail
            .tool_input_summary
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(input) => format!("{tool}: {input}"),
            None => tool.to_string(),
        }
    } else {
        detail
            .last_message
            .as_deref()
            .filter(|m| !m.is_empty())?
            .to_string()
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
    is_focused: bool,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let _ = is_active;
    let status = row.effective_status();
    // Reference-cockpit compact row: "{primary} - {secondary}", one line.
    //   primary   = the user's prompt, else the agent's name ("Claude Code")
    //   secondary = the live tool/message, else the state label ("Idle")
    // primary leads brighter; secondary trails muted; the state indicator (check
    // / spinner ring / colored dot) carries the status. The row ALWAYS truncates
    // to one line — the reference cockpit never wraps an agent row, active or not.
    let primary_is_prompt = live_prompt(row).is_some();
    let primary = collapse_ws(&live_prompt(row).unwrap_or_else(|| row.label.to_string()));
    let secondary = collapse_ws(&live_activity(row).unwrap_or_else(|| agent_state_label(&status).to_string()));
    let show_secondary = !secondary.is_empty() && secondary != primary;
    // The row truncates to one line, so the full "{primary} - {secondary}" is
    // unreadable when it overflows. A hover tooltip restores it — mirroring the
    // reference cockpit's native `title` on the compact row.
    let tooltip_text: SharedString = if show_secondary {
        format!("{primary} - {secondary}")
    } else {
        primary.clone()
    }
    .into();
    // The focused row's strong fill would wash out the dimmed text, so lift both
    // tones toward full foreground when focused — mirroring the reference
    // cockpit's `isFocusedPane` text treatment.
    let primary_color = if is_focused || primary_is_prompt {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let secondary_color = if is_focused {
        theme.fg_muted
    } else {
        theme.fg_subtle
    };

    // One line holding two inline-styled spans: a brighter primary (the prompt)
    // and a dimmer " - secondary" (the live tool/state). The prompt is the
    // identity of the row, so it leads at its natural width; only the trailing
    // secondary yields and ellipsises first when the row narrows — matching the
    // reference cockpit, which truncates the concatenation from the end. The
    // primary still truncates if it alone overflows.
    let descriptor = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .child(
            div()
                .min_w_0()
                .flex_shrink()
                .text_size(px(typography.t_body_sm))
                .text_color(primary_color)
                .truncate()
                .child(primary),
        )
        .when(show_secondary, |d| {
            d.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(typography.t_body_sm))
                    .text_color(secondary_color)
                    .truncate()
                    .child(format!(" - {secondary}")),
            )
        });

    // A stable element id makes GPUI track this row's hover hitbox across the
    // rail's frequent re-renders, so the hover fill repaints promptly on
    // mouse-move (a bare `.hover()` div without an id repaints unreliably).
    // Mirrors the `.id()`-stateful workspace card.
    let base = div()
        .id(SharedString::from(format!("agent-row-{}", row.db_id)))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        .pl(px(density.pad_panel * 3.0))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline))
        // Focused row keeps a resting fill (a "stuck hover"), like the reference
        // cockpit's focused-pane row; non-focused rows light only on hover.
        .when(is_focused, |d| d.bg(theme.hover_overlay))
        .child(agent_state_indicator(
            &status,
            is_completed_turn(row),
            &row.db_id,
            theme,
        ))
        .child(
            svg()
                .path(adapter_icon_path(&row.adapter_id))
                .size(px(13.0))
                .text_color(theme.fg_muted)
                .flex_shrink_0(),
        )
        .child(descriptor)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography.t_sub_label))
                .text_color(secondary_color)
                .child(row.age_label.clone()),
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, cx)
        });
    if row.is_focusable() {
        let target = row.target.clone();
        base.cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak_root.update(cx, |root, cx| match &target {
                    RailAgentTarget::AgentSession { db_id } => {
                        root.focus_agent_by_db_id(db_id, window, cx);
                    }
                    RailAgentTarget::AmbientTerminal { pty_id } => {
                        root.focus_ambient_agent_terminal(pty_id, window, cx);
                    }
                });
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
            target: RailAgentTarget::AgentSession { db_id: "id".into() },
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
            persisted_message: None,
            ambient_detail: None,
        }
    }

    #[test]
    fn collapsed_chips_group_by_color_and_order_by_severity() {
        let theme = Theme::default();
        // Two warn-bucket states (Running + NeedsApproval) must merge into one
        // amber chip; the chips come back error → warn → ok → muted.
        let rows = vec![
            row(false, AgentStatus::Idle, None),
            row(false, AgentStatus::Running, None),
            row(false, AgentStatus::NeedsApproval("approve?".into()), None),
            row(false, AgentStatus::Done { code: Some(0) }, None),
            row(false, AgentStatus::Failed("boom".into()), None),
        ];
        let chips = collapsed_state_chips(&rows, theme);
        // Severity order error → warn → ok → muted; each chip carries its
        // agents' adapters (all "claude-code" here). Only the done bucket is a
        // check glyph.
        let colors: Vec<_> = chips.iter().map(|c| (c.color, c.adapters.len())).collect();
        assert_eq!(
            colors,
            vec![
                (theme.status_error, 1), // failed
                (theme.status_warn, 2),  // running + needs-approval merged
                (theme.status_ok, 1),    // done
                (theme.status_muted, 1), // idle
            ]
        );
        assert_eq!(
            chips.iter().map(|c| c.is_done).collect::<Vec<_>>(),
            vec![false, false, true, false]
        );
    }

    #[test]
    fn collapsed_chips_skip_empty_buckets() {
        let theme = Theme::default();
        // All idle → a single muted chip carrying both agents, no empty buckets.
        let rows = vec![
            row(false, AgentStatus::Idle, None),
            row(false, AgentStatus::Idle, None),
        ];
        let chips = collapsed_state_chips(&rows, theme);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].color, theme.status_muted);
        assert_eq!(chips[0].adapters.len(), 2);
        assert!(!chips[0].is_done);
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

    /// A LIVE idle row whose channel carries the given detail (the post-Stop
    /// snapshot: status Idle + last assistant message).
    fn idle_row_with_detail(detail: oximux_core::SidebandDetail) -> RailAgentRow {
        let (tx, rx) = live_rx(AgentStatus::Idle);
        tx.send(AgentSnapshot {
            status: AgentStatus::Idle,
            detail: Some(detail),
        })
        .unwrap();
        row(true, AgentStatus::Idle, Some(rx))
    }

    #[test]
    fn finished_turn_idle_row_is_done_and_shows_the_message() {
        // Idle + an assistant reply = the reference cockpit's "done": the row
        // must report completed (emerald check) and surface the reply as its
        // secondary, NOT the bare "Idle" state label.
        let detail = oximux_core::SidebandDetail {
            prompt: Some("hi".into()),
            last_message: Some("All set — tests pass.".into()),
            ..Default::default()
        };
        let r = idle_row_with_detail(detail);
        assert!(is_completed_turn(&r), "idle + reply = finished turn");
        assert_eq!(live_prompt(&r).as_deref(), Some("hi"));
        assert_eq!(live_activity(&r).as_deref(), Some("All set — tests pass."));
    }

    #[test]
    fn fresh_idle_row_without_a_reply_is_not_done() {
        // A just-launched agent that has never completed a turn (no reply) stays
        // a grey idle row, like the reference cockpit's fresh "Claude Code - Idle".
        let r = row(true, AgentStatus::Idle, None);
        assert!(!is_completed_turn(&r));
        assert_eq!(live_activity(&r), None);
    }

    #[test]
    fn ambient_row_reads_detail_for_message_and_done_state() {
        // An ambient row has no status channel; its tool/message come from the
        // carried `ambient_detail`. A completed ambient turn reads as done too.
        let mut r = row(true, AgentStatus::Idle, None);
        r.ambient_detail = Some(oximux_core::SidebandDetail {
            prompt: Some("hi 2".into()),
            last_message: Some("Ready when you are.".into()),
            ..Default::default()
        });
        assert!(is_completed_turn(&r));
        assert_eq!(live_prompt(&r).as_deref(), Some("hi 2"));
        assert_eq!(live_activity(&r).as_deref(), Some("Ready when you are."));
    }

    #[test]
    fn restored_row_surfaces_the_persisted_message_when_live_channel_is_empty() {
        // A re-adopted tracked row: live status channel is empty (fresh poll
        // loop), but the DB-persisted reply must restore the finished-turn
        // message + done-check across a restart, not revert to "Idle".
        let mut r = row(true, AgentStatus::Idle, None);
        r.persisted_message = Some("Done — 3 files changed.".into());
        assert!(is_completed_turn(&r), "persisted reply ⇒ finished turn");
        assert_eq!(live_activity(&r).as_deref(), Some("Done — 3 files changed."));
    }

    #[test]
    fn live_message_wins_over_a_stale_persisted_message() {
        // When the agent runs a new turn after restore, the live reply must take
        // precedence over the persisted one.
        let (tx, rx) = live_rx(AgentStatus::Idle);
        tx.send(AgentSnapshot {
            status: AgentStatus::Idle,
            detail: Some(oximux_core::SidebandDetail {
                last_message: Some("Fresh reply.".into()),
                ..Default::default()
            }),
        })
        .unwrap();
        let mut r = row(true, AgentStatus::Idle, Some(rx));
        r.persisted_message = Some("Old persisted reply.".into());
        assert_eq!(live_activity(&r).as_deref(), Some("Fresh reply."));
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
