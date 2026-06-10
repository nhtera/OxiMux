//! Single-row painter for the agents dashboard.
//!
//! Each row is laid out as a single nowrap horizontal strip:
//!
//!   [status dot] [project · slug/branch] [flex spacer] [verb chip] [+A −B chip?]
//!
//! Nowrap keeps the row from reflowing inside `uniform_list` so horizontal
//! scroll works correctly when the rail is narrow.

use gpui::{Div, InteractiveElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::model::{AgentRow, attention_rank};
use crate::shell::left_rail::workspace_row::STATUS_DOT_SIZE;

/// Row height used for the `uniform_list` item height. Matches workspace rows
/// so the rail looks visually consistent when switching between views.
pub(crate) const ROW_HEIGHT: f32 = 38.0;

/// Paint one `AgentRow` into a GPUI element. Height is `ROW_HEIGHT` so all
/// rows have the same fixed height for `uniform_list`.
///
/// Layout (single horizontal line, nowrap):
///   status dot · project label · separator · slug · spacer · verb · diff chip
pub fn render_agent_row(
    row: &AgentRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    // Status dot — reuses STATUS_DOT_SIZE from workspace_row for visual parity.
    let dot = div()
        .size(px(STATUS_DOT_SIZE))
        .rounded_full()
        .bg(row.verb.color)
        .flex_shrink_0();

    // "ProjectName · slug" or "ProjectName · branch" label.
    let branch_or_slug = if !row.workspace.branch.is_empty() {
        row.workspace.branch.as_str()
    } else {
        row.workspace.slug.as_str()
    };
    let loc_text = format!("{} · {}", row.project_name, branch_or_slug);
    let loc_label = div()
        .flex_1()
        .min_w_0()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .overflow_hidden()
        .whitespace_nowrap()
        .child(loc_text);

    // Verb chip (colored label e.g. "Running", "Waiting for input").
    let verb_chip = div()
        .flex_shrink_0()
        .text_size(px(typography.t_sub_label))
        .text_color(row.verb.color)
        .whitespace_nowrap()
        .child(row.verb.label);

    // Review affordance: action-required rows (NeedsApproval / WaitingForInput)
    // get a discoverable call-to-action. Clicking anywhere on the row already
    // jumps to the agent's tab (the only place the user can act — approval is
    // handled in the agent's own TUI, not programmatically); this chip just
    // makes that affordance visible. Tinted with the verb color for urgency.
    let review_chip = (attention_rank(row.status.as_ref(), row.is_live) == 0).then(|| {
        div()
            .flex_shrink_0()
            .ml(px(density.gap_inline))
            .px(px(density.gap_inline))
            .rounded(px(density.r_chip))
            .border_1()
            .border_color(row.verb.color)
            .text_size(px(typography.t_sub_label))
            .text_color(row.verb.color)
            .whitespace_nowrap()
            .child("Review →")
    });

    // Diff chip: "+A −B" — omitted when diff is absent or both counts are zero.
    let diff_chip = row.diff.as_ref().and_then(|d| {
        if d.added == 0 && d.removed == 0 {
            return None;
        }
        let label = format!("+{} −{}", d.added, d.removed);
        Some(
            div()
                .flex_shrink_0()
                .ml(px(density.gap_inline))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(label),
        )
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .child(dot)
        .child(loc_label)
        .child(verb_chip)
        .children(review_chip)
        .children(diff_chip)
}
