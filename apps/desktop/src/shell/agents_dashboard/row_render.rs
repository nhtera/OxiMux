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

use crate::shell::agents_dashboard::model::{AgentRow, attention_rank};
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
                .rounded(px(6.))
                .bg(theme.hover_overlay)
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(count.to_string()),
        )
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
    // Tier 0 = the user is being asked something (approval / input).
    let is_attention = attention_rank(row.status.as_ref(), row.is_live) == 0;

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

    // ── line 2: the live tool step / reply, else the state label ──
    let secondary = row
        .secondary
        .clone()
        .unwrap_or_else(|| agent_state_label(&status).to_string());
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
