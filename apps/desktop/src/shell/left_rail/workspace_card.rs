//! GPUI card painter for the rich two-line workspace card.
//!
//! Consumes a `WorkspaceCardPlan` (pure, computed by `workspace_row.rs`) and
//! emits the GPUI element tree. Kept in this file so `workspace_row.rs` stays
//! under the 200-LOC soft cap.
//!
//! Layout (two lines):
//!   Line 1: [dot] [name] [agent glyph, compact only] [primary badge] [branch chip]
//!   Line 2: [agent verb (colored)] [+A −B diff chip]
//!
//! Card height is documented as a local exception in `design-guidelines.md`
//! (2 × `h_row` to fit two lines). Hover quick-actions (the "…" menu button)
//! are preserved from the original row painter.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AppContext, ElementId, Entity, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    div, prelude::FluentBuilder, px, svg,
};
use gpui_component::input::{Enter as InputEnter, Escape as InputEscape, Input, InputState};
use oximux_core::{WorkPhase, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::LeftRail;
use crate::shell::left_rail::project_drag::{
    SidebarDragPreview, WorkspaceDragConfig, WorkspaceDragPayload, insertion_side,
    paint_insertion_line,
};

/// Inline-rename wiring for one workspace row. Built by `project_group` for
/// every non-primary row.
pub struct RowRenameConfig {
    /// The rail entity, for the begin / commit / cancel callbacks.
    pub rail: Entity<LeftRail>,
    /// This row's workspace — the double-click "begin rename" payload.
    pub workspace: Workspace,
    /// `Some(field)` when THIS row is the one being renamed: the title is
    /// replaced by this edit field. `None` = render the title with a
    /// double-click-to-rename affordance.
    pub active_input: Option<Entity<InputState>>,
}
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

/// The brand glyph a workspace card shows to name the agent running on it, or
/// `None` when something else already names it.
///
/// Only the compact layout needs one. Detailed spells the agent out on line 2
/// (`Codex · Ready`), and compact drops that line to fit a single row — which
/// left the status dot as the card's only agent signal, and a dot says how an
/// agent is doing, never which one it is. A multi-agent workspace needs none
/// either: its disclosure renders below the card in both layouts and names
/// every agent.
///
/// An agent with no bundled mark falls back to the generic glyph rather than
/// showing nothing — that an agent is there is the more important half.
fn compact_agent_glyph(
    compact: bool,
    has_agent_disclosure: bool,
    agent_name: Option<&str>,
) -> Option<&'static str> {
    if !compact || has_agent_disclosure {
        return None;
    }
    let adapter = crate::shell::agent_presentation::adapter_id_for_label(agent_name?);
    Some(crate::shell::agent_presentation::adapter_icon_path(adapter))
}

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
    active_shell: bool,
    // When a multi-agent disclosure renders below this card, the per-agent rows
    // already carry name + status, so the card drops its redundant
    // `name · verb` line-2 summary (matches the reference cockpit, which shows
    // the branch then "N agents" rather than repeating an agent summary).
    suppress_agent_summary: bool,
    show_menu: bool,
    locate_glow_seq: u64,
    drag: Option<WorkspaceDragConfig>,
    rename: Option<RowRenameConfig>,
    // Single-line compact layout: drops the second line (agent verb / diff)
    // and shrinks the card to one row height. Detailed (two-line) when false.
    compact: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_row_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    on_menu_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let menu_id: SharedString = format!("{row_id}-menu").into();

    // The menu-open callback is reached from two affordances — the `…` button
    // and a right-click on the row — so it is shared via `Rc`.
    let on_menu_click = std::rc::Rc::new(on_menu_click);
    let on_menu_click_btn = on_menu_click.clone();

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
                on_menu_click_btn(ev, window, cx);
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

    // Work-phase chip: the agent's declared position in the task, beside the
    // issue badge. Colored by phase so a glance across the rail separates
    // "someone is on this" from "this is waiting on me" without reading.
    // Absent for an unset — or unrecognised — phase; see `WorkspaceCardPlan`.
    let phase_chip = plan.phase.map(|phase| {
        let color = match phase {
            WorkPhase::Todo => theme.status_muted,
            WorkPhase::InProgress => theme.status_info,
            WorkPhase::InReview => theme.status_warning,
            WorkPhase::Done => theme.status_added,
        };
        div()
            .flex()
            .items_center()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_chip))
            .bg(theme.bg_overlay)
            .text_size(px(typography.t_sub_label))
            .text_color(color)
            .child(phase.label())
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

    // Agent brand glyph — names the agent when the layout has nowhere else to.
    // See [`compact_agent_glyph`] for when that is.
    let agent_glyph = compact_agent_glyph(
        compact,
        suppress_agent_summary,
        plan.agent_name.as_deref(),
    )
    .map(|icon| {
        svg()
            .path(icon)
            .size(px(12.0))
            .flex_shrink_0()
            // An svg with no explicit text color paints nothing.
            .text_color(theme.fg_muted)
    });

    // Pin glyph — a small marker on pinned rows. Sits right after the name so
    // it reads as a property of the row, distinct from the leading status dot.
    let pin_indicator = plan.pinned.then(|| {
        svg()
            .path("icons/pin.svg")
            .size(px(11.0))
            .flex_shrink_0()
            .text_color(theme.fg_muted)
    });

    // Name slot — normally the row title, but swapped for an inline edit field
    // while this row is being renamed. A non-active rename config makes the
    // title double-clickable to begin a rename.
    let name_element: gpui::AnyElement = match rename {
        // This row is actively being renamed → show the edit field. Enter/Escape
        // are captured before the Input's own handling so Escape stays a cancel
        // (it clears the rename first, so the resulting blur is a no-op).
        Some(RowRenameConfig {
            rail,
            active_input: Some(input),
            ..
        }) => {
            let rail_commit = rail.clone();
            let rail_cancel = rail.clone();
            div()
                .flex_1()
                .min_w_0()
                .capture_action(move |_: &InputEnter, _window, cx| {
                    rail_commit.update(cx, |r, cx| r.commit_rename(cx));
                })
                .capture_action(move |_: &InputEscape, _window, cx| {
                    rail_cancel.update(cx, |r, cx| r.cancel_rename(cx));
                })
                .child(Input::new(&input))
                .into_any_element()
        }
        // Renamable row at rest → title with double-click-to-rename.
        Some(RowRenameConfig {
            rail, workspace, ..
        }) => div()
            .text_size(px(typography.t_body_sm))
            .text_color(plan.row.fg)
            .child(plan.row.name.clone())
            .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                if ev.click_count >= 2 {
                    cx.stop_propagation();
                    let workspace = workspace.clone();
                    rail.update(cx, |r, cx| r.begin_rename_workspace(workspace, window, cx));
                }
            })
            .into_any_element(),
        // Primary / non-renamable row → plain title.
        None => div()
            .text_size(px(typography.t_body_sm))
            .text_color(plan.row.fg)
            .child(plan.row.name.clone())
            .into_any_element(),
    };

    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(name_element)
        .children(agent_glyph)
        .children(pin_indicator)
        .children(primary_badge)
        .children(branch_chip)
        .children(issue_chip)
        .children(phase_chip)
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

    // A live agent's prompt is its title — it replaces the `name · verb`
    // summary as the primary line-2 text (the dot still carries the status),
    // matching the reference cockpit's prompt-as-title rows. Kept in a flex-row
    // with `min_w_0` + `.truncate()` so a prompt wider than the rail clips to
    // one line with an ellipsis instead of wrapping (a flex-col text pitfall).
    let title_elem = plan.agent_title.as_ref().map(|title| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_base)
                    .truncate()
                    .child(title.clone()),
            )
    });

    // The worktree's own progress line, in the same slot and shape as the live
    // title. Truncates identically — one line, ellipsis, never wrapping.
    let comment_elem = plan.comment.as_ref().map(|comment| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_base)
                    .truncate()
                    .child(comment.clone()),
            )
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

    // When a live title is present it takes the whole line (the prompt is the
    // headline); otherwise fall back to the `name · verb` summary. The diff
    // chip rides along either way.
    let line2 = if comment_elem.is_some() {
        // A progress line the agent wrote about itself outranks the prompt it
        // was handed: the prompt says what was asked, the comment says where
        // the work actually is. The dot still carries live status either way.
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(density.gap_inline))
            .children(comment_elem)
            .children(diff_elem)
    } else if title_elem.is_some() {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(density.gap_inline))
            .children(title_elem)
            .children(diff_elem)
    } else if suppress_agent_summary {
        // Multi-agent: the disclosure below lists each agent, so line 2 drops
        // the `name · verb` summary and carries only the diff chip.
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .children(diff_elem)
    } else {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .children(name_elem)
            .children(verb_elem)
            .children(diff_elem)
    };

    // Card shell — two-line tall. Active cards render inset with a rounded
    // border; inactive cards sit flush and lift on hover. Mirrors the
    // existing workspace row active/inactive treatment.
    // No explicit `w_full` here: the flex-column wrapper (below) stretches the
    // card to fill the rail width on the cross axis, which — unlike `w_full` —
    // correctly shrinks the box by the active row's horizontal margins instead
    // of overflowing past the right edge (which would clip the right corners).
    let base = div()
        .id(row_id)
        .group(group_name)
        .flex()
        .flex_row()
        .items_center()
        .h(px(if compact {
            density.h_row
        } else {
            density.h_row * CARD_HEIGHT_MULT
        }))
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

    // Every row uses the same inset + rounded card shape so the active fill and
    // the hover highlight share one geometry (matching the reference app): the
    // active row adds a border + persistent fill, an inactive row stays
    // transparent at rest and only tints (still rounded + inset) on hover. Both
    // are inset by the same margin, so content alignment never shifts between
    // states.
    //
    // Only a LEFT margin is applied (not `mx`): the rail's right edge is taken
    // up by the resize handle's hit-pad (an equal-width strip of rail surface),
    // so the card runs flush to the body's right edge and that hit-pad becomes
    // the right gap — keeping the visible left/right gaps symmetric. The flex
    // wrapper means this no longer overflows, so all four corners still round.
    let shell = if plan.row.is_active && active_shell {
        // Single-agent active card uses the same solid active border as the
        // multi-agent wrapper so the selection reads consistently regardless
        // of how many agents are running on the workspace.
        base.ml(px(density.gap_inline))
            .rounded(px(density.r_card))
            .border_1()
            .border_color(theme.border_active)
            .bg(plan.row.bg)
    } else if plan.row.is_active {
        base.rounded(px(density.r_card))
    } else {
        base.ml(px(density.gap_inline))
            .rounded(px(density.r_card))
            .hover(|s| s.bg(theme.hover_overlay))
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
                // Compact mode shows only the title line; the second line
                // (agent verb / diff) is dropped to fit a single row height.
                .when(!compact, |c| c.child(line2)),
        )
        .children(trailing_btn)
        .on_mouse_down(MouseButton::Left, on_row_click)
        // Right-click opens the same row popover at the cursor (DRY with the
        // `…` button). Gated to rows that have a menu (non-primary).
        .when(show_menu, |el| {
            el.on_mouse_down(MouseButton::Right, move |ev, window, cx| {
                cx.stop_propagation();
                on_menu_click(ev, window, cx);
            })
        })
        // Drag-to-reorder (Manual mode, non-primary rows only). Stateless
        // idiom: the payload carries the source index, `drag_over` paints the
        // insertion line, `on_drop` validates same-group then persists.
        .when_some(drag, |el, cfg| {
            let this_index = cfg.src_index;
            let accent = cfg.accent;
            let ghost = cfg.ghost_label.clone();
            let over_project = cfg.project_id.clone();
            let drop_project = cfg.project_id.clone();
            let this_workspace_id = cfg.workspace_id.clone();
            let on_reorder = cfg.on_reorder.clone();
            el.on_drag(
                WorkspaceDragPayload {
                    workspace_id: cfg.workspace_id.clone(),
                    project_id: cfg.project_id.clone(),
                    src_index: cfg.src_index,
                },
                move |_p, _offset, _window, cx| {
                    cx.new(|_| SidebarDragPreview::new(ghost.clone()))
                },
            )
            .drag_over::<WorkspaceDragPayload>(move |style, payload, _window, _cx| {
                // Only react to rows from the same group; a foreign-group drag
                // never lands here (drop rejects it), so draw no line.
                if payload.project_id != over_project || payload.src_index == this_index {
                    return style;
                }
                paint_insertion_line(
                    style,
                    insertion_side(payload.src_index, this_index),
                    accent,
                    theme.bg_rail,
                )
            })
            .on_drop::<WorkspaceDragPayload>(
                move |payload: &WorkspaceDragPayload, window, cx| {
                    // Reorder-only: reject a drop from a different project group or
                    // onto the source row itself.
                    if payload.project_id != drop_project
                        || payload.workspace_id == this_workspace_id
                    {
                        return;
                    }
                    on_reorder(
                        payload.workspace_id.clone(),
                        this_workspace_id.clone(),
                        window,
                        cx,
                    );
                },
            )
        });

    // Locate glow: the scroll-to-current affordance replays a one-shot
    // ring fade over the ACTIVE card, keyed on the bump sequence so it
    // runs exactly once per trigger. Same recipe as the pane rim-flash:
    // a dedicated absolute overlay animates its border alpha to zero and
    // leaves no residue. seq == 0 means never triggered (and reduced
    // motion never bumps the seq).
    // Match the active card's left inset + radius so the ring traces the card
    // edge, not the full-width wrapper.
    let glow_overlay = (plan.row.is_active && active_shell && locate_glow_seq > 0).then(|| {
        locate_glow_overlay(
            locate_glow_seq,
            px(density.r_card),
            px(density.gap_inline),
            theme.focus_ring,
        )
    });

    // Outer wrapper carries the tint accent so it sits at a consistent
    // rail-left for every row (the active card's own margin doesn't shift it).
    // Flex-column so the in-flow card stretches to the wrapper width minus its
    // own horizontal margins (the absolute tint/glow children stay out of flow).
    div()
        .relative()
        .flex()
        .flex_col()
        .w_full()
        .children(tint_bar)
        .child(card)
        .children(glow_overlay)
}

/// One-shot locate "blink": an absolute ring overlay whose border alpha fades
/// to zero, keyed on `locate_glow_seq` so it runs exactly once per trigger.
/// Same recipe as the pane rim-flash. Shared by the single-agent active card
/// and the multi-agent active wrapper so both pulse identically when the
/// scroll-to-current affordance fires. The host element must be positioned
/// (`relative`); `left_margin` insets the ring to trace a card edge inside a
/// full-width wrapper — pass `px(0.)` when the host already carries that inset
/// (the multi-agent wrapper does, via its own `ml`).
pub(crate) fn locate_glow_overlay(
    locate_glow_seq: u64,
    radius: gpui::Pixels,
    left_margin: gpui::Pixels,
    ring: Hsla,
) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .ml(left_margin)
        .rounded(radius)
        .border_1()
        .with_animation(
            ElementId::NamedInteger("locate-glow".into(), locate_glow_seq),
            Animation::new(Duration::from_millis(LOCATE_GLOW_MS))
                .with_easing(gpui::ease_out_quint()),
            move |el, delta| el.border_color(Hsla { a: 1.0 - delta, ..ring }),
        )
}

#[cfg(test)]
mod tests {
    use super::compact_agent_glyph;

    #[test]
    fn a_compact_card_names_its_agent_with_a_brand_glyph() {
        assert_eq!(
            compact_agent_glyph(true, false, Some("Codex")),
            Some("icons/codex.svg")
        );
        assert_eq!(
            compact_agent_glyph(true, false, Some("Claude Code")),
            Some("icons/claude-code.svg")
        );
    }

    #[test]
    fn an_agent_with_no_bundled_mark_still_shows_that_it_is_there() {
        // Gemini/Grok/Droid ship no glyph yet; a missing brand must not read
        // as a missing agent.
        assert_eq!(
            compact_agent_glyph(true, false, Some("Gemini CLI")),
            Some("icons/sparkles.svg")
        );
    }

    #[test]
    fn the_detailed_layout_needs_no_glyph() {
        // Line 2 already reads `Codex · Ready` there.
        assert_eq!(compact_agent_glyph(false, false, Some("Codex")), None);
    }

    #[test]
    fn a_workspace_with_a_disclosure_needs_no_glyph() {
        // The disclosure renders below the card in BOTH layouts and names
        // every agent; a single glyph beside the name would contradict it.
        assert_eq!(compact_agent_glyph(true, true, Some("Codex")), None);
    }

    #[test]
    fn a_workspace_with_no_agent_shows_no_glyph() {
        assert_eq!(compact_agent_glyph(true, false, None), None);
    }
}
