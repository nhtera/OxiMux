//! Render for `ProjectPanes`: recursively walks the group tree and
//! emits a flex layout where Split nodes become divider-separated rows
//! / columns and Leaf nodes become `PaneGroup` entities.
//!
//! Pure view construction — no mutation. Drag-resize between sibling
//! groups is deferred to a later slice (OQ2 in the step-06 plan); for
//! v1 splits render at fixed 50/50.

use gpui::{
    AnyElement, App, AppContext, Context, DragMoveEvent, Entity, ExternalPaths, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px,
};

use std::collections::HashSet;

use super::{ProjectPanes, TabDragHoveredTarget};
use crate::shell::pane_group::file_drag::FilePathDragPayload;
use crate::shell::pane_group::render::{build_tab_strip_for, filter_droppable_files};
use crate::shell::pane_group::tab_drag::TabDragPayload;
use crate::shell::pane_group::tab_drag_zones::{Zone, resolve_drop_zone};
use crate::shell::pane_tree::{Axis, PaneGroupId, PaneTree};

/// Visible width of a divider line, in pixels. Matches the in-group
/// divider so the chrome reads as one consistent stripe.
const DIVIDER_THICKNESS_PX: f32 = 1.0;

/// Hit area added on each side of the visible divider so cursor pickup
/// is forgiving — the visible line stays 1 px but the draggable hitbox
/// is `DIVIDER_THICKNESS_PX + 2 * DIVIDER_HIT_PAD_PX` wide.
const DIVIDER_HIT_PAD_PX: f32 = 3.0;

/// Drag payload for a group-divider resize. Carries the path to the
/// split node, which divider within it (children[i] / children[i+1]),
/// the split axis, and the weights captured at drag-start so the
/// move handler can recompute new weights without losing precision
/// across many fast moves.
#[derive(Clone, Debug)]
struct DividerDragPayload {
    split_path: Vec<usize>,
    divider_idx: usize,
    axis: Axis,
    initial_weights: Vec<f32>,
}

/// Zero-sized placeholder for the drag visual. GPUI requires `on_drag`
/// to return an Entity that renders the floating cursor preview; for a
/// resize handle there's no preview — the divider itself moves on every
/// `on_drag_move` tick.
struct DividerDragGhost;

impl Render for DividerDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

/// Walk the tree's TOP horizontal-layer leaves with their weights.
///
/// "Top layer" rules — each group's strip below is hoisted into the
/// workspace's top bar row IF its leaf is in this top layer. Lower
/// vertical-split rows keep their strips inline above their bodies.
///
///  * Leaf node → that leaf alone (weight 1.0).
///  * Horizontal split → recurse into every child; their weights flow
///    into the top-bar split layout.
///  * Vertical split → only the FIRST child contributes (the visually-
///    topmost row); its sub-tree is then walked as the new root.
///
/// Returned weights are RAW (post-multiplication) so callers can splice
/// them straight into a flex layout that mirrors body column widths.
fn collect_top_row_leaves(node: &PaneTree<PaneGroupId>) -> Vec<(PaneGroupId, f32)> {
    fn walk(node: &PaneTree<PaneGroupId>, weight: f32, out: &mut Vec<(PaneGroupId, f32)>) {
        match node {
            PaneTree::Leaf(id) => out.push((*id, weight)),
            PaneTree::Split {
                axis: Axis::Horizontal,
                children,
                weights,
            } => {
                let sum: f32 = weights.iter().sum();
                let sum = if sum > 0.0 { sum } else { 1.0 };
                for (child, w) in children.iter().zip(weights.iter()) {
                    walk(child, weight * (w / sum), out);
                }
            }
            PaneTree::Split {
                axis: Axis::Vertical,
                children,
                weights,
            } => {
                // Only the topmost row hoists. Its sub-tree may itself
                // contain horizontal splits, which we keep walking.
                if let (Some(first), Some(first_weight)) = (children.first(), weights.first()) {
                    let sum: f32 = weights.iter().sum();
                    let _ = sum; // unused — vertical weight stays full at this level
                    let _ = first_weight; // vertical weight collapses; horizontal weight passes through unchanged
                    walk(first, weight, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(node, 1.0, &mut out);
    out
}

impl ProjectPanes {
    /// Build the hoisted top-bar strip row. Mirrors the tree's top
    /// horizontal layer — each top-row leaf renders its strip in a slot
    /// whose width matches that leaf's column in the body below. Lower
    /// vertical-split rows keep their strips inline (rendered by
    /// `render_tree`'s inline branch).
    pub fn topmost_tab_strip(&self, cx: &App) -> Option<AnyElement> {
        let tree = self.manager().group_tree();
        let entries = collect_top_row_leaves(tree);
        if entries.is_empty() {
            return None;
        }
        let theme = self.theme;
        let active_id = self.manager().active_group_id();
        let sum: f32 = entries.iter().map(|(_, w)| *w).sum();
        let sum = if sum > 0.0 { sum } else { 1.0 };
        // flex_1 + min_w(0) matches the spacer pattern in
        // `top_bar::center_header` so the row absorbs the entire
        // available width between left chrome and right toggles.
        let mut row = div()
            .flex()
            .flex_row()
            .items_stretch()
            .h_full()
            .flex_1()
            .min_w(px(0.0));
        for (id, weight) in entries {
            let Some(entity) = self.group(id) else {
                continue;
            };
            let frac = weight / sum;
            let is_focused = id == active_id;
            let strip = build_tab_strip_for(entity, id, is_focused, true, theme, cx);
            row = row.child(
                div()
                    .w(gpui::relative(frac))
                    .h_full()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(strip),
            );
        }
        Some(row.into_any_element())
    }

    /// Set of group ids whose strips are hoisted into the top bar.
    /// `render_tree` consults this to skip the inline strip for those
    /// leaves — they'd otherwise paint twice (once hoisted, once inline).
    fn hoisted_leaf_ids(&self) -> HashSet<PaneGroupId> {
        collect_top_row_leaves(self.manager().group_tree())
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }
}

impl Render for ProjectPanes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drop any group whose tabs got closed down to zero. Refusing
        // the last group keeps the workspace anchored.
        self.purge_empty_groups(window, cx);

        // Stale-hover guard: GPUI fires on_drop only when the user
        // releases over a listening target. Drops onto empty space,
        // off-window releases, or drag-cancel (Escape, app switch) just
        // dump `cx.active_drag` to None with no callback. Without this
        // check the last-painted 5-zone overlay stays on screen until
        // the next pointer move triggers a re-render.
        if !cx.has_active_drag() && self.hovered_drop_target().is_some() {
            self.set_hovered_drop_target(None, cx);
        }

        let theme = self.theme;
        let tree = self.manager().group_tree().clone();
        let active_group_id = self.manager().active_group_id();
        let hovered = self.hovered_drop_target();
        let entity = cx.entity().clone();
        // Top-row leaves hoist their strip into the workspace's top bar
        // (`topmost_tab_strip`); inline-strip rendering below skips them
        // so the strip never paints twice.
        let hoisted = self.hoisted_leaf_ids();
        let body = render_tree(
            &tree,
            self,
            active_group_id,
            hovered,
            entity,
            theme,
            &hoisted,
            &[],
            cx,
        );
        div().flex().flex_col().size_full().child(body)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_tree(
    node: &PaneTree<PaneGroupId>,
    panes: &ProjectPanes,
    active_group_id: PaneGroupId,
    hovered: Option<TabDragHoveredTarget>,
    project_panes_entity: Entity<ProjectPanes>,
    theme: oximux_settings::Theme,
    hoisted: &HashSet<PaneGroupId>,
    path: &[usize],
    cx: &App,
) -> AnyElement {
    match node {
        PaneTree::Leaf(id) => match panes.group(*id) {
            Some(group) => {
                let slot = div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden();
                let is_focused = *id == active_group_id;
                let body_zone = hovered.and_then(|h| (h.group_id == *id).then_some(h.zone));
                let group_body = leaf_body(
                    *id,
                    group.clone(),
                    body_zone,
                    project_panes_entity.clone(),
                    theme,
                    is_focused,
                );
                // Hoisted leaves render BARE bodies — their strip is
                // already painted in the workspace top bar (see
                // `topmost_tab_strip`). Non-hoisted leaves (e.g. a
                // vertical-split's bottom row) keep their strip inline.
                if hoisted.contains(id) {
                    slot.child(group_body).into_any_element()
                } else {
                    slot.child(build_tab_strip_for(group, *id, is_focused, true, theme, cx))
                        .child(group_body)
                        .into_any_element()
                }
            }
            // Leaf in the tree but no entity registered — shouldn't
            // happen in well-formed state, but rendering an empty slot
            // is safer than panicking inside a render closure.
            None => div().size_full().into_any_element(),
        },
        PaneTree::Split {
            axis,
            children,
            weights,
        } => {
            let split_axis = *axis;
            // Path owned for the move handler so it survives the render
            // closure dropping. Cheap clone — usually 0..3 entries deep.
            let split_path = path.to_vec();
            let weights_snapshot = weights.clone();
            let move_path = split_path.clone();
            let move_axis = split_axis;
            let move_panes = project_panes_entity.clone();
            let mut row = div()
                .flex()
                .size_full()
                .overflow_hidden()
                .on_drag_move::<DividerDragPayload>(
                    move |ev: &DragMoveEvent<DividerDragPayload>, _window, cx| {
                        // Snapshot what we need from the active drag BEFORE
                        // calling cx.update: `ev.drag(cx)` borrows the payload
                        // immutably from `cx.active_drag`, and `update`
                        // requires mutable access to the same `cx`.
                        let (split_path, divider_idx, initial_weights, p_axis) = {
                            let payload = ev.drag(cx);
                            (
                                payload.split_path.clone(),
                                payload.divider_idx,
                                payload.initial_weights.clone(),
                                payload.axis,
                            )
                        };
                        // Each split row registers a move listener; the
                        // payload's path filters back to the originating
                        // row so unrelated splits don't fire.
                        if split_path != move_path || p_axis != move_axis {
                            return;
                        }
                        let bounds = ev.bounds;
                        let pos = ev.event.position;
                        let (axis_pos, axis_size) = match move_axis {
                            Axis::Horizontal => (
                                f32::from(pos.x - bounds.origin.x),
                                f32::from(bounds.size.width),
                            ),
                            Axis::Vertical => (
                                f32::from(pos.y - bounds.origin.y),
                                f32::from(bounds.size.height),
                            ),
                        };
                        if axis_size <= 0.0 {
                            return;
                        }
                        let frac = (axis_pos / axis_size).clamp(0.0, 1.0);
                        let new_weights = redistribute_weights(&initial_weights, divider_idx, frac);
                        move_panes.update(cx, |p, cx| {
                            p.set_split_weights(&split_path, new_weights, cx);
                        });
                    },
                );
            row = match split_axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            let sum: f32 = weights_snapshot.iter().sum();
            let sum = if sum > 0.0 { sum } else { 1.0 };
            for (i, (child, weight)) in children.iter().zip(weights_snapshot.iter()).enumerate() {
                let frac = (weight / sum) * 100.0;
                // Slot must be a flex container so the inner leaf/split
                // child (which uses size_full) gets a real box to fill.
                let mut slot = div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden();
                slot = match split_axis {
                    Axis::Horizontal => slot.w(gpui::relative(frac / 100.0)).h_full(),
                    Axis::Vertical => slot.h(gpui::relative(frac / 100.0)).w_full(),
                };
                let mut child_path = split_path.clone();
                child_path.push(i);
                row = row.child(slot.child(render_tree(
                    child,
                    panes,
                    active_group_id,
                    hovered,
                    project_panes_entity.clone(),
                    theme,
                    hoisted,
                    &child_path,
                    cx,
                )));
                if i + 1 < children.len() {
                    row = row.child(interactive_divider(
                        split_path.clone(),
                        i,
                        split_axis,
                        weights_snapshot.clone(),
                        theme,
                    ));
                }
            }
            row.into_any_element()
        }
    }
}

/// Compute new weights when the user drags divider `divider_idx` to
/// `frac` of the parent split (0.0 = pinned left/top edge, 1.0 = right/
/// bottom). Only the two children adjacent to the divider are redistributed
/// — siblings on either side keep their initial weight, matching the
/// pane-resize semantics every other tiling tool uses. The returned
/// vector still passes through `PaneTree::set_split_weights`, which
/// clamps each entry to `MIN_FLEX` so a drag can't shrink a pane to zero.
fn redistribute_weights(initial: &[f32], divider_idx: usize, frac: f32) -> Vec<f32> {
    let mut out = initial.to_vec();
    if divider_idx + 1 >= initial.len() {
        return out;
    }
    let sum: f32 = initial.iter().sum();
    if sum <= 0.0 {
        return out;
    }
    let before_pair: f32 = initial[..divider_idx].iter().sum();
    let pair_sum = initial[divider_idx] + initial[divider_idx + 1];
    if pair_sum <= 0.0 {
        return out;
    }
    let target_total = frac * sum;
    let frac_in_pair = ((target_total - before_pair) / pair_sum).clamp(0.0, 1.0);
    out[divider_idx] = pair_sum * frac_in_pair;
    out[divider_idx + 1] = pair_sum * (1.0 - frac_in_pair);
    out
}

/// Interactive resize handle between two sibling groups. The visible
/// stripe is 1 px (matching `divider`) but a transparent padded area on
/// each side widens the hitbox for forgiving cursor pickup. Dragging
/// fires `on_drag` with a `DividerDragPayload`; the matching
/// `on_drag_move` lives on the parent split row (which captures the
/// parent's pixel bounds via `ev.bounds`).
fn interactive_divider(
    path: Vec<usize>,
    divider_idx: usize,
    axis: Axis,
    weights: Vec<f32>,
    theme: oximux_settings::Theme,
) -> AnyElement {
    let id_string = format!(
        "divider-{}-{}",
        path.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("-"),
        divider_idx,
    );
    let payload = DividerDragPayload {
        split_path: path,
        divider_idx,
        axis,
        initial_weights: weights,
    };
    let stripe = div().flex_shrink_0().bg(theme.border_active);
    let stripe = match axis {
        Axis::Horizontal => stripe.w(px(DIVIDER_THICKNESS_PX)).h_full(),
        Axis::Vertical => stripe.h(px(DIVIDER_THICKNESS_PX)).w_full(),
    };
    let mut handle = div()
        .id(SharedString::from(id_string))
        .flex()
        .flex_shrink_0()
        .occlude();
    handle = match axis {
        Axis::Horizontal => handle
            .flex_row()
            .items_center()
            .justify_center()
            .h_full()
            .w(px(DIVIDER_THICKNESS_PX + 2.0 * DIVIDER_HIT_PAD_PX))
            .cursor_col_resize(),
        Axis::Vertical => handle
            .flex_col()
            .items_center()
            .justify_center()
            .w_full()
            .h(px(DIVIDER_THICKNESS_PX + 2.0 * DIVIDER_HIT_PAD_PX))
            .cursor_row_resize(),
    };
    handle
        .child(stripe)
        .on_drag(payload, |_payload, _offset, _window, cx| {
            cx.new(|_| DividerDragGhost)
        })
        .into_any_element()
}

/// Wrap a `PaneGroup` entity in its body container — a stateful div
/// that handles the drag-to-split `on_drag_move` + `on_drop` events.
/// The active 5-zone overlay (if any) renders as an absolutely-
/// positioned semi-transparent child.
fn leaf_body(
    group_id: PaneGroupId,
    group: Entity<crate::shell::pane_group::PaneGroup>,
    active_zone: Option<Zone>,
    project_panes: Entity<ProjectPanes>,
    theme: oximux_settings::Theme,
    is_focused: bool,
) -> AnyElement {
    let body_id = SharedString::from(format!("pane-group-body-{}", group.entity_id()));
    let hover_panes = project_panes.clone();
    let drop_panes = project_panes.clone();
    let file_hover_panes = project_panes.clone();
    let file_drop_panes = project_panes.clone();
    // F4.6: separate clones for the OS-native (Finder) drop variant.
    let native_hover_panes = project_panes.clone();
    let native_drop_panes = project_panes.clone();
    // Subtle dim on the unfocused group body so the eye finds the focused
    // group faster. Tab strip stays at full opacity — only the content
    // area dims (matches the reference editor's `opacity-95` pattern).
    let body_opacity: f32 = if is_focused { 1.0 } else { 0.95 };

    div()
        .id(body_id)
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .relative()
        .overflow_hidden()
        .opacity(body_opacity)
        .on_drag_move::<TabDragPayload>(move |ev: &DragMoveEvent<TabDragPayload>, _window, cx| {
            // Zone resolver wants the LOCAL position inside the body
            // bounds; the event carries window-space coords + the
            // body's window-space bounds, so `resolve_drop_zone`
            // subtracts the origin internally.
            let bounds = ev.bounds;
            if !bounds.contains(&ev.event.position) {
                return;
            }
            let payload = ev.drag(cx);
            // Suppress overlay when dropping a tab back on its own
            // group's body would be a layout no-op (single-tab group
            // dragged onto itself can't split anywhere visible).
            if payload.source_group == group_id {
                let source_tabs = hover_panes
                    .read(cx)
                    .group(group_id)
                    .map(|g| g.read(cx).tab_count())
                    .unwrap_or(0);
                if source_tabs <= 1 {
                    hover_panes.update(cx, |p, cx| p.set_hovered_drop_target(None, cx));
                    return;
                }
            }
            let zone = resolve_drop_zone(bounds, ev.event.position);
            hover_panes.update(cx, |p, cx| {
                p.set_hovered_drop_target(Some(TabDragHoveredTarget { group_id, zone }), cx);
            });
        })
        .on_drop::<TabDragPayload>(move |payload: &TabDragPayload, window, cx| {
            // Resolve the zone from the live hover state (set by the
            // most-recent on_drag_move). If somehow stale (no hover
            // recorded but a drop arrived) bail safely.
            let zone = drop_panes
                .read(cx)
                .hovered_drop_target()
                .filter(|t| t.group_id == group_id)
                .map(|t| t.zone);
            let Some(zone) = zone else {
                drop_panes.update(cx, |p, cx| p.set_hovered_drop_target(None, cx));
                return;
            };
            let source = payload.source_group;
            let source_tab_idx = payload.source_tab_idx;
            drop_panes.update(cx, |p, cx| {
                p.set_hovered_drop_target(None, cx);
                match zone {
                    Zone::Center => {
                        // Merge into the target group's strip. No-op
                        // when source == target — that case is
                        // already filtered above in on_drag_move so
                        // the overlay never appears.
                        p.transfer_tab(source, source_tab_idx, group_id, window, cx);
                    }
                    Zone::Left | Zone::Right | Zone::Up | Zone::Down => {
                        p.split_and_move_tab(source, source_tab_idx, group_id, zone, window, cx);
                    }
                }
            });
        })
        // File-row drag-drop: same overlay + zone math, payload carries a
        // filesystem path instead of a (group, tab) reference. GPUI
        // dispatches by TypeId so these handlers never cross-fire with
        // the `TabDragPayload` pair above.
        .on_drag_move::<FilePathDragPayload>(
            move |ev: &DragMoveEvent<FilePathDragPayload>, _window, cx| {
                let bounds = ev.bounds;
                if !bounds.contains(&ev.event.position) {
                    return;
                }
                let zone = resolve_drop_zone(bounds, ev.event.position);
                file_hover_panes.update(cx, |p, cx| {
                    p.set_hovered_drop_target(Some(TabDragHoveredTarget { group_id, zone }), cx);
                });
            },
        )
        .on_drop::<FilePathDragPayload>(move |payload: &FilePathDragPayload, window, cx| {
            let zone = file_drop_panes
                .read(cx)
                .hovered_drop_target()
                .filter(|t| t.group_id == group_id)
                .map(|t| t.zone);
            let Some(zone) = zone else {
                file_drop_panes.update(cx, |p, cx| p.set_hovered_drop_target(None, cx));
                return;
            };
            let path = payload.path.clone();
            file_drop_panes.update(cx, |p, cx| {
                p.set_hovered_drop_target(None, cx);
                match zone {
                    Zone::Center => {
                        p.open_file_in_group(group_id, path, window, cx);
                    }
                    Zone::Left | Zone::Right | Zone::Up | Zone::Down => {
                        p.split_and_open_file(group_id, zone, path, window, cx);
                    }
                }
            });
        })
        // F4.6: OS-native (Finder) drop variant for body-zone targeting.
        // GPUI translates Finder file drops into an internal drag with
        // `ExternalPaths` payload, so we mirror the FilePathDragPayload
        // pair above. Multi-file drops open in sequence in the same zone.
        .on_drag_move::<ExternalPaths>(move |ev: &DragMoveEvent<ExternalPaths>, _window, cx| {
            let bounds = ev.bounds;
            if !bounds.contains(&ev.event.position) {
                return;
            }
            let zone = resolve_drop_zone(bounds, ev.event.position);
            native_hover_panes.update(cx, |p, cx| {
                p.set_hovered_drop_target(Some(TabDragHoveredTarget { group_id, zone }), cx);
            });
        })
        .on_drop::<ExternalPaths>(move |payload: &ExternalPaths, window, cx| {
            let zone = native_drop_panes
                .read(cx)
                .hovered_drop_target()
                .filter(|t| t.group_id == group_id)
                .map(|t| t.zone);
            let Some(zone) = zone else {
                native_drop_panes.update(cx, |p, cx| p.set_hovered_drop_target(None, cx));
                return;
            };
            let paths = filter_droppable_files(payload.paths());
            native_drop_panes.update(cx, |p, cx| {
                p.set_hovered_drop_target(None, cx);
                // First path drives the split-vs-merge zone semantics.
                // Remaining paths (multi-file Finder drop) fall back
                // to opening as regular tabs in the resulting group —
                // the split itself already happened on path[0].
                let mut iter = paths.into_iter();
                if let Some(first) = iter.next() {
                    match zone {
                        Zone::Center => {
                            p.open_file_in_group(group_id, first, window, cx);
                        }
                        Zone::Left | Zone::Right | Zone::Up | Zone::Down => {
                            p.split_and_open_file(group_id, zone, first, window, cx);
                        }
                    }
                    for extra in iter {
                        p.open_file_in_group(group_id, extra, window, cx);
                    }
                }
            });
        })
        .child(group)
        .when_some(active_zone, |s, zone| s.child(zone_overlay(zone, theme)))
        .into_any_element()
}

/// Semi-transparent overlay rectangle for the active drop zone.
/// Sized per zone — half-width / half-height for edge zones, full body
/// for center merge. Positioned absolutely inside the body container.
fn zone_overlay(zone: Zone, theme: oximux_settings::Theme) -> impl IntoElement {
    let base = div()
        .absolute()
        .bg(theme.focus_ring)
        .opacity(0.18)
        .border_2()
        .border_color(theme.focus_ring);
    match zone {
        Zone::Center => base.inset_0(),
        Zone::Left => base.top_0().left_0().h_full().w(gpui::relative(0.5)),
        Zone::Right => base.top_0().right_0().h_full().w(gpui::relative(0.5)),
        Zone::Up => base.top_0().left_0().w_full().h(gpui::relative(0.5)),
        Zone::Down => base.bottom_0().left_0().w_full().h(gpui::relative(0.5)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redistribute_two_pane_at_midpoint_is_half() {
        let new_w = redistribute_weights(&[1.0, 1.0], 0, 0.5);
        assert!((new_w[0] - 1.0).abs() < 1e-3);
        assert!((new_w[1] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn redistribute_two_pane_drags_to_quarter() {
        let new_w = redistribute_weights(&[1.0, 1.0], 0, 0.25);
        let total = new_w.iter().sum::<f32>();
        assert!((new_w[0] / total - 0.25).abs() < 1e-3);
        assert!((new_w[1] / total - 0.75).abs() < 1e-3);
    }

    #[test]
    fn redistribute_three_pane_leaves_outer_pane_untouched() {
        // [1, 1, 1] split — drag divider 1 (between children 1 and 2)
        // to 0.8 of the parent. Children 0 keeps its initial weight; only
        // children 1 and 2 redistribute.
        let new_w = redistribute_weights(&[1.0, 1.0, 1.0], 1, 0.8);
        assert!((new_w[0] - 1.0).abs() < 1e-3, "outer pane untouched");
        let pair_sum = new_w[1] + new_w[2];
        assert!((pair_sum - 2.0).abs() < 1e-3, "pair sum preserved");
        let total: f32 = new_w.iter().sum();
        // Divider sits at fraction `target_total / total` of total weight.
        let divider_frac = (new_w[0] + new_w[1]) / total;
        assert!((divider_frac - 0.8).abs() < 1e-3, "divider lands at 0.8");
    }

    #[test]
    fn redistribute_clamps_below_zero() {
        // Dragging past the leading edge collapses left to 0; PaneTree's
        // set_split_weights then clamps to MIN_FLEX on apply.
        let new_w = redistribute_weights(&[1.0, 1.0], 0, -0.5);
        assert!(new_w[0].abs() < 1e-3);
        assert!((new_w[1] - 2.0).abs() < 1e-3);
    }

    #[test]
    fn redistribute_no_op_on_invalid_divider_idx() {
        // divider_idx out of range → returns initial weights unchanged.
        let new_w = redistribute_weights(&[1.0, 1.0], 5, 0.5);
        assert_eq!(new_w, vec![1.0, 1.0]);
    }
}
