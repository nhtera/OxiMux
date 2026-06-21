//! Single-row painter for the agents dashboard.
//!
//! Each row is a two-line card:
//!
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ (◐)  project · branch            verb-chip      +A −B     │  line 1
//!   │      Bash: cargo test… / Review →                         │  line 2
//!   └──────────────────────────────────────────────────────────┘
//!
//! The leading element is a 28px icon ring whose border carries the status
//! color and whose glyph is the agent's brand mark. Action-required rows
//! (tier 0: NeedsApproval / WaitingForInput) get floating-card chrome plus a
//! 2px left accent bar so they read as "come look at me".

use gpui::{Div, InteractiveElement, ParentElement, Styled, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::model::{AgentRow, attention_rank, sideband_subline};
use crate::ui::overlay::FloatingSurface;

/// Row height for the `uniform_list` item. Taller than the old single-line
/// row to fit the two-line card + 28px icon ring with breathing room.
pub(crate) const ROW_HEIGHT: f32 = 52.0;

/// Diameter of the agent icon ring.
const ICON_RING_SIZE: f32 = 28.0;
/// Size of the brand glyph inside the ring.
const ICON_GLYPH_SIZE: f32 = 16.0;

/// Paint one `AgentRow` into a GPUI element. Height is `ROW_HEIGHT` so all
/// rows have the same fixed height for `uniform_list`.
pub fn render_agent_row(
    row: &AgentRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    let verb_color = row.verb.color;
    // Tier 0 = the user is being asked something (approval / input).
    let is_attention = attention_rank(row.status.as_ref(), row.is_live) == 0;

    // Icon ring: status-color border around the agent's brand glyph. The
    // glyph is tinted to the same status color so the ring reads as one mark.
    let icon_ring = div()
        .flex_shrink_0()
        .size(px(ICON_RING_SIZE))
        .rounded_full()
        .border_1()
        .border_color(verb_color)
        .bg(theme.bg_panel)
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .path(row.icon_path)
                .size(px(ICON_GLYPH_SIZE))
                .text_color(verb_color),
        );

    // ── line 1: "Project · branch" (truncates) + verb chip + diff chip ──
    let branch_or_slug = if !row.workspace.branch.is_empty() {
        row.workspace.branch.as_str()
    } else {
        row.workspace.slug.as_str()
    };
    let loc_label = div()
        .flex_1()
        .min_w_0()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .overflow_hidden()
        .whitespace_nowrap()
        .child(format!("{} · {}", row.project_name, branch_or_slug));

    let verb_chip = div()
        .flex_shrink_0()
        .text_size(px(typography.t_sub_label))
        .text_color(verb_color)
        .whitespace_nowrap()
        .child(row.verb.label);

    // Diff chip: "+A −B" — omitted when diff is absent or both counts are zero.
    let diff_chip = row.diff.as_ref().and_then(|d| {
        if d.added == 0 && d.removed == 0 {
            return None;
        }
        Some(
            div()
                .flex_shrink_0()
                .ml(px(density.gap_inline))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(format!("+{} −{}", d.added, d.removed)),
        )
    });

    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(density.gap_inline))
        .child(loc_label)
        .child(verb_chip)
        .children(diff_chip);

    // ── line 2: review CTA (tier 0) else the live tool subline ──
    // Action-required rows surface a discoverable "Review →" — clicking the
    // row jumps to the agent's tab (approval happens in its own TUI). Running
    // rows show the structured sideband tool step ("Edit · src/lib.rs") when
    // the agent reports one, falling back to the log-tailed activity line;
    // everything else leaves line 2 empty (the taller card still reads cleanly).
    let line2_child = if is_attention {
        Some(
            div()
                .min_w_0()
                .text_size(px(typography.t_sub_label))
                .text_color(verb_color)
                .overflow_hidden()
                .whitespace_nowrap()
                .child("Review →"),
        )
    } else {
        let subline = row
            .sideband_detail
            .as_ref()
            .and_then(sideband_subline)
            .or_else(|| row.activity.clone());
        subline.map(|text| {
            div()
                .min_w_0()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(text)
        })
    };
    let line2 = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .children(line2_child);

    // Stacked text column next to the icon ring.
    let text_col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(2.))
        .child(line1)
        .child(line2);

    let root = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .child(icon_ring)
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
