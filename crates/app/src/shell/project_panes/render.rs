//! Render for `ProjectPanes`: recursively walks the group tree and
//! emits a flex layout where Split nodes become divider-separated rows
//! / columns and Leaf nodes become `PaneGroup` entities.
//!
//! Pure view construction — no mutation. Drag-resize between sibling
//! groups is deferred to a later slice (OQ2 in the step-06 plan); for
//! v1 splits render at fixed 50/50.

use gpui::{
    AnyElement, App, Context, DragMoveEvent, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};

use std::collections::HashSet;

use super::{ProjectPanes, TabDragHoveredTarget};
use crate::shell::pane_group::render::build_tab_strip_for;
use crate::shell::pane_group::tab_drag::TabDragPayload;
use crate::shell::pane_group::tab_drag_zones::{Zone, resolve_drop_zone};
use crate::shell::pane_tree::{Axis, PaneGroupId, PaneTree};

/// Visible width of a divider line, in pixels. Matches the in-group
/// divider so the chrome reads as one consistent stripe.
const DIVIDER_THICKNESS_PX: f32 = 1.0;

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
                if let (Some(first), Some(first_weight)) =
                    (children.first(), weights.first())
                {
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
                );
                // Hoisted leaves render BARE bodies — their strip is
                // already painted in the workspace top bar (see
                // `topmost_tab_strip`). Non-hoisted leaves (e.g. a
                // vertical-split's bottom row) keep their strip inline.
                if hoisted.contains(id) {
                    slot.child(group_body).into_any_element()
                } else {
                    slot.child(build_tab_strip_for(
                        group,
                        *id,
                        is_focused,
                        true,
                        theme,
                        cx,
                    ))
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
            let mut row = div().flex().size_full().overflow_hidden();
            row = match axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            let sum: f32 = weights.iter().sum();
            let sum = if sum > 0.0 { sum } else { 1.0 };
            for (i, (child, weight)) in children.iter().zip(weights.iter()).enumerate() {
                let frac = (weight / sum) * 100.0;
                // Slot must be a flex container so the inner leaf/split
                // child (which uses size_full) gets a real box to fill.
                let mut slot = div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden();
                slot = match axis {
                    Axis::Horizontal => slot.w(gpui::relative(frac / 100.0)).h_full(),
                    Axis::Vertical => slot.h(gpui::relative(frac / 100.0)).w_full(),
                };
                row = row.child(slot.child(render_tree(
                    child,
                    panes,
                    active_group_id,
                    hovered,
                    project_panes_entity.clone(),
                    theme,
                    hoisted,
                    cx,
                )));
                if i + 1 < children.len() {
                    row = row.child(divider(*axis, theme));
                }
            }
            row.into_any_element()
        }
    }
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
) -> AnyElement {
    let body_id = SharedString::from(format!("pane-group-body-{}", group.entity_id()));
    let hover_panes = project_panes.clone();
    let drop_panes = project_panes.clone();

    div()
        .id(body_id)
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .relative()
        .overflow_hidden()
        .on_drag_move::<TabDragPayload>(
            move |ev: &DragMoveEvent<TabDragPayload>, _window, cx| {
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
                    p.set_hovered_drop_target(
                        Some(TabDragHoveredTarget {
                            group_id,
                            zone,
                        }),
                        cx,
                    );
                });
            },
        )
        .on_drop::<TabDragPayload>(
            move |payload: &TabDragPayload, window, cx| {
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
                            p.split_and_move_tab(
                                source,
                                source_tab_idx,
                                group_id,
                                zone,
                                window,
                                cx,
                            );
                        }
                    }
                });
            },
        )
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
        Zone::Left => base
            .top_0()
            .left_0()
            .h_full()
            .w(gpui::relative(0.5)),
        Zone::Right => base
            .top_0()
            .right_0()
            .h_full()
            .w(gpui::relative(0.5)),
        Zone::Up => base
            .top_0()
            .left_0()
            .w_full()
            .h(gpui::relative(0.5)),
        Zone::Down => base
            .bottom_0()
            .left_0()
            .w_full()
            .h(gpui::relative(0.5)),
    }
}

fn divider(axis: Axis, theme: oximux_settings::Theme) -> AnyElement {
    let base = div().flex_shrink_0().bg(theme.border_active);
    match axis {
        Axis::Horizontal => base
            .w(px(DIVIDER_THICKNESS_PX))
            .h_full()
            .into_any_element(),
        Axis::Vertical => base
            .h(px(DIVIDER_THICKNESS_PX))
            .w_full()
            .into_any_element(),
    }
}
