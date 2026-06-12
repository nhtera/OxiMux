//! GPUI card painter for the rich two-line workspace card.
//!
//! Consumes a `WorkspaceCardPlan` (pure, computed by `workspace_row.rs`) and
//! emits the GPUI element tree. Kept in this file so `workspace_row.rs` stays
//! under the 200-LOC soft cap.
//!
//! Layout (two lines):
//!   Line 1: [dot] [name] [primary badge] [branch chip]
//!   Line 2: [agent verb (colored)] [+A −B diff chip]
//!
//! Card height is documented as a local exception in `design-guidelines.md`
//! (2 × `h_row` to fit two lines). Hover quick-actions (the "…" menu button)
//! are preserved from the original row painter.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, ElementId, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::workspace_row::{
    FOLDER_ICON_SIZE, STATUS_DOT_SIZE, TRAILING_BTN_SIZE, WorkspaceCardPlan,
};

/// Card height: two content lines plus padding. Expressed as a multiplier of
/// `density.h_row` rather than an absolute pixel value so it scales with the
/// density system. Local exception: documented in `design-guidelines.md`
/// "Approved exceptions" table alongside `ROW_HEIGHT_MULT = 1.6`.
const CARD_HEIGHT_MULT: f32 = 2.2;

/// Locate-glow duration. Deliberately OUTSIDE the sub-200ms motion
/// vocabulary: this is a one-shot "you are here" locator that must linger
/// long enough to catch an eye that's still travelling from the button.
const LOCATE_GLOW_MS: u64 = 1500;

/// Render the rich two-line workspace card.
///
/// `row_id` and `group_name` must be stable and unique per workspace — callers
/// typically derive them from the workspace id, matching the existing row
/// pattern in `project_group.rs`.
#[allow(clippy::too_many_arguments)]
pub fn render_workspace_card(
    plan: WorkspaceCardPlan,
    row_id: SharedString,
    group_name: SharedString,
    show_menu: bool,
    locate_glow_seq: u64,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_row_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    on_menu_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let menu_id: SharedString = format!("{row_id}-menu").into();

    // Trailing "…" button — invisible at rest, revealed on row hover via
    // `group_hover`. Primary (main worktree) rows suppress this because the
    // main worktree is removed by removing the project, not here.
    let trailing_btn = show_menu.then(|| {
        div()
            .id(menu_id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(TRAILING_BTN_SIZE))
            .rounded(px(density.r_xs))
            .text_color(theme.fg_muted)
            .invisible()
            .group_hover(group_name.clone(), |s| s.visible())
            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
            .child(
                svg()
                    .path("icons/ellipsis.svg")
                    .size(px(FOLDER_ICON_SIZE))
                    .text_color(theme.fg_muted),
            )
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new("Workspace actions").build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                cx.stop_propagation();
                on_menu_click(ev, window, cx);
            })
    });

    // Line 1 — name + optional primary badge + optional branch chip.
    let primary_badge = (plan.row.is_primary && !plan.row.is_folder).then(|| {
        div()
            .flex()
            .items_center()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_xs))
            .border_1()
            .border_color(theme.border_inactive)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child("primary")
    });

    // Branch chip: shown when branch is present and this is not a folder project.
    let branch_chip = plan.branch.as_ref().map(|branch| {
        div()
            .flex()
            .items_center()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_chip))
            .bg(theme.bg_overlay)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(branch.clone())
    });

    // Linked-issue badge: shown when the workspace was created from a task
    // (e.g. "#42"). Tinted status_info to distinguish it from the branch chip.
    let issue_chip = plan.linked_issue.as_ref().map(|issue| {
        div()
            .flex()
            .items_center()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_chip))
            .bg(theme.bg_overlay)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.status_info)
            .child(issue.clone())
    });

    // "Folder" pill for non-git folder projects — shown in line 1 subtext slot.
    let folder_pill = plan.row.is_folder.then(|| {
        div()
            .flex()
            .items_center()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_xs))
            .bg(theme.bg_overlay)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child("Folder")
    });

    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(plan.row.fg)
                .child(plan.row.name),
        )
        .children(primary_badge)
        .children(branch_chip)
        .children(issue_chip)
        .children(folder_pill);

    // Line 2 — [agent name ·] agent verb + diff chip. All optional; when
    // absent the line collapses to empty (card stays two-row tall).
    // The agent name (a tracked session's adapter, or a hand-launched agent
    // detected from its terminal title) precedes the verb: "Claude Code · Running".
    let name_elem = plan.agent_name.as_ref().map(|name| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_muted)
                    .truncate()
                    .child(name.clone()),
            )
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .child("·"),
            )
    });

    let verb_elem = plan.agent_verb.as_ref().map(|v| {
        div()
            .text_size(px(typography.t_sub_label))
            .text_color(v.color)
            .child(v.label)
    });

    // Diff chip: "+A −B" using status_added / status_removed colors.
    // Clean worktrees (0/0) suppress the chip — an all-zero stat row is
    // noise on every resting workspace.
    let diff_elem = plan
        .diff
        .as_ref()
        .filter(|d| d.added > 0 || d.removed > 0)
        .map(|d| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.status_added)
                    .child(format!("+{}", d.added)),
            )
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.status_removed)
                    .child(format!("−{}", d.removed)),
            )
    });

    let line2 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .children(name_elem)
        .children(verb_elem)
        .children(diff_elem);

    // Card shell — two-line tall. Active cards render inset with a rounded
    // border; inactive cards sit flush and lift on hover. Mirrors the
    // existing workspace row active/inactive treatment.
    let base = div()
        .id(row_id)
        .group(group_name)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row * CARD_HEIGHT_MULT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer();

    // Thin (2px) left-edge identifier hue — the only per-workspace chrome
    // tint, drawn as an accent, never a fill (design contract). Painted on an
    // outer wrapper (below) so it pins to the same rail-left for every row,
    // whether or not the active row is inset by its margin.
    let tint_bar = plan.tint.map(|c| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(px(2.0))
            .bg(gpui::rgb(c.rgb()))
    });

    let shell = if plan.row.is_active {
        base.mx(px(density.gap_inline))
            .rounded(px(density.r_card))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(plan.row.bg)
    } else {
        base.bg(plan.row.bg).hover(|s| s.bg(theme.hover_overlay))
    };

    let card = shell
        .child(
            div()
                .size(px(STATUS_DOT_SIZE))
                .rounded_full()
                .bg(plan.row.dot_color)
                .flex_shrink_0(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(line1)
                .child(line2),
        )
        .children(trailing_btn)
        .on_mouse_down(MouseButton::Left, on_row_click);

    // Locate glow: the scroll-to-current affordance replays a one-shot
    // ring fade over the ACTIVE card, keyed on the bump sequence so it
    // runs exactly once per trigger. Same recipe as the pane rim-flash:
    // a dedicated absolute overlay animates its border alpha to zero and
    // leaves no residue. seq == 0 means never triggered (and reduced
    // motion never bumps the seq).
    let glow_overlay = (plan.row.is_active && locate_glow_seq > 0).then(|| {
        let ring = theme.focus_ring;
        div()
            .absolute()
            .inset_0()
            // Match the active card's inset + radius so the ring traces
            // the card edge, not the full-width wrapper.
            .mx(px(density.gap_inline))
            .rounded(px(density.r_card))
            .border_1()
            .with_animation(
                ElementId::NamedInteger("locate-glow".into(), locate_glow_seq),
                Animation::new(Duration::from_millis(LOCATE_GLOW_MS))
                    .with_easing(gpui::ease_out_quint()),
                move |el, delta| el.border_color(Hsla { a: 1.0 - delta, ..ring }),
            )
    });

    // Outer wrapper carries the tint accent so it sits at a consistent
    // rail-left for every row (the active card's own margin doesn't shift it).
    div()
        .relative()
        .w_full()
        .children(tint_bar)
        .child(card)
        .children(glow_overlay)
}
