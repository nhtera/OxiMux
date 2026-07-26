//! Render for `ProjectPanes`: recursively walks the group tree and
//! emits a flex layout where Split nodes become divider-separated rows
//! / columns and Leaf nodes become `PaneGroup` entities.
//!
//! View construction is otherwise pure; the one exception is two render-phase
//! fields (`rim_flash_token`, `last_active_group`) updated in place to detect a
//! cross-group focus move for the focused-pane rim flash. Those updates never
//! call `notify()`, so they can't re-trigger a render.

use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, ElementId, Entity,
    ExternalPaths, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use std::time::Duration;

use std::collections::HashSet;

use super::{ProjectPanes, TabDragHoveredTarget};
use crate::shell::divider::{ActiveDivider, DividerBoundsCache, split_row_bounds_canvas};
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
    pub fn topmost_tab_strip(
        &self,
        project_panes: Entity<ProjectPanes>,
        cx: &App,
    ) -> Option<AnyElement> {
        let tree = self.manager().group_tree();
        let entries = collect_top_row_leaves(tree);
        if entries.is_empty() {
            return None;
        }
        // Skip the entire hoisted strip when EVERY top-row group has
        // zero tabs — drops the focused-group blue border, the empty
        // chip rail, and the trailing "+ / ..." cluster so the chrome
        // row collapses to a clean single header matching the
        // reference editor's empty-workspace UX. Empty groups inside a
        // vertical split still exist (purge keeps the last group), so
        // we check live tab counts rather than group presence.
        if entries
            .iter()
            .all(|(id, _)| self.group(*id).is_none_or(|g| g.read(cx).is_empty()))
        {
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
            let strip = build_tab_strip_for(
                entity,
                id,
                is_focused,
                true,
                theme,
                self.workspace_tint,
                project_panes.clone(),
                cx,
            );
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

        // Focused-pane rim flash: when focus moves to a *different* group while
        // the layout is split, bump the flash token so the newly focused leaf
        // replays a brief focus_ring rim. Single-group layouts don't flash —
        // there's no ambiguity about where focus is. The very first focus
        // (no prior group) is skipped so a fresh window doesn't flash on open.
        let is_split = self.manager().in_order_groups().len() > 1;
        self.rim_flash_token = advance_rim_flash(
            self.last_active_group,
            active_group_id,
            is_split,
            self.rim_flash_token,
        );
        self.last_active_group = Some(active_group_id);
        let rim_flash = is_split.then_some(self.rim_flash_token);

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
            rim_flash,
            hovered,
            entity,
            theme,
            &hoisted,
            &[],
            cx,
        );
        // While a divider is held, a topmost occluding overlay captures
        // all moves (so the resize tracks past the divider hitbox and no
        // stray events reach the terminals beneath) until release.
        let capture = self
            .active_divider()
            .map(|active| divider_capture_overlay(cx.entity().clone(), active.axis));
        div()
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .child(body)
            .when_some(capture, |s, overlay| s.child(overlay))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_tree(
    node: &PaneTree<PaneGroupId>,
    panes: &ProjectPanes,
    active_group_id: PaneGroupId,
    rim_flash: Option<u64>,
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
                    .relative()
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
                // Brief focus-ring rim when this leaf just gained focus in a
                // split. `rim_flash` carries the per-focus token (None when not
                // split); only the focused leaf paints it.
                let flash = rim_flash
                    .filter(|_| is_focused)
                    .map(|token| rim_flash_overlay(*id, token, theme));
                // Hoisted leaves render BARE bodies — their strip is
                // already painted in the workspace top bar (see
                // `topmost_tab_strip`). Non-hoisted leaves (e.g. a
                // vertical-split's bottom row) keep their strip inline.
                if hoisted.contains(id) {
                    slot.child(group_body)
                        .when_some(flash, |s, f| s.child(f))
                        .into_any_element()
                } else {
                    slot.child(build_tab_strip_for(
                        group,
                        *id,
                        is_focused,
                        true,
                        theme,
                        panes.workspace_tint,
                        project_panes_entity.clone(),
                        cx,
                    ))
                    .child(group_body)
                    .when_some(flash, |s, f| s.child(f))
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
            // Path owned for the bounds canvas / divider handlers so it
            // survives the render closure dropping. Cheap clone — usually
            // 0..3 entries deep.
            let split_path = path.to_vec();
            let weights_snapshot = weights.clone();
            // `.relative()` anchors the bounds-recording canvas (absolute,
            // size_full) to this split row so it measures the row's own
            // geometry. The canvas records into the shared cache keyed by
            // `split_path`; the divider MouseDown looks it up to seed the
            // mouse-capture resize.
            let mut row = div().flex().relative().size_full().overflow_hidden();
            row = match split_axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            row = row.child(split_row_bounds_canvas(
                panes.divider_bounds_cache(),
                split_path.clone(),
            ));
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
                    rim_flash,
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
                        project_panes_entity.clone(),
                        panes.divider_bounds_cache(),
                    ));
                }
            }
            row.into_any_element()
        }
    }
}

/// Decide the next rim-flash token. Bumps only when focus actually moved to a
/// different group while the layout is split AND there was a prior focused
/// group (so a freshly opened window doesn't flash on first render). A bumped
/// token restarts the flash animation; an unchanged token lets the current
/// flash settle. Pure so the trigger rule is unit-testable without GPUI.
fn advance_rim_flash(
    prev: Option<PaneGroupId>,
    current: PaneGroupId,
    is_split: bool,
    token: u64,
) -> u64 {
    if is_split && prev.is_some() && prev != Some(current) {
        token.wrapping_add(1)
    } else {
        token
    }
}

/// Quick focus-ring rim drawn over a leaf the instant it gains focus in a
/// split. Animates from a visible ring to fully transparent (~0.28s) so it
/// reads as a flash and leaves no residue — the persistent focus marker stays
/// the tab strip's bottom border. `token` keys the animation id so every fresh
/// focus replays it; a stable token lets a continuously-focused leaf settle at
/// transparent rather than re-flashing each frame.
fn rim_flash_overlay(id: PaneGroupId, token: u64, theme: oximux_settings::Theme) -> AnyElement {
    let ring = theme.focus_ring;
    div()
        .absolute()
        .inset_0()
        .border_2()
        .border_color(ring)
        .with_animation(
            ElementId::Name(format!("pane-rim-flash-{}-{}", id.0, token).into()),
            // ease-out so the rim drops fast then settles — reads as a flash,
            // not a lingering fade.
            Animation::new(Duration::from_secs_f32(0.28)).with_easing(gpui::ease_out_quint()),
            move |el, delta| el.border_color(Hsla { a: 1.0 - delta, ..ring }),
        )
        .into_any_element()
}

/// Compute new weights when the user drags divider `divider_idx` to
/// `frac` of the parent split (0.0 = pinned left/top edge, 1.0 = right/
/// bottom). Only the two children adjacent to the divider are redistributed
/// — siblings on either side keep their initial weight, matching the
/// pane-resize semantics every other tiling tool uses. The returned
/// vector still passes through `PaneTree::set_split_weights`, which
/// clamps each entry to `MIN_FLEX` so a drag can't shrink a pane to zero.
pub(super) fn redistribute_weights(initial: &[f32], divider_idx: usize, frac: f32) -> Vec<f32> {
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
/// each side widens the hitbox for forgiving cursor pickup. Resize uses
/// direct mouse capture: MouseDown arms the divider on `ProjectPanes`
/// (seeding the parent split-row bounds from `bounds_cache`), a topmost
/// capture overlay resizes on MouseMove, and MouseUp disarms. Double-click
/// resets the split to equal weights.
#[allow(clippy::too_many_arguments)]
fn interactive_divider(
    path: Vec<usize>,
    divider_idx: usize,
    axis: Axis,
    weights: Vec<f32>,
    theme: oximux_settings::Theme,
    panes: Entity<ProjectPanes>,
    bounds_cache: DividerBoundsCache,
) -> AnyElement {
    let id_string = format!(
        "divider-{}-{}",
        path.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("-"),
        divider_idx,
    );
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
    handle.child(stripe).on_mouse_down(
        MouseButton::Left,
        move |ev: &MouseDownEvent, _window, cx| {
            // Double-click resets the split to equal weights — don't arm.
            if ev.click_count >= 2 {
                let path = path.clone();
                panes.update(cx, |p, cx| p.reset_split_to_equal(&path, cx));
                cx.stop_propagation();
                return;
            }
            // First frame may have no cached bounds yet — ignore the press
            // (the next one will have a populated cache).
            let Some(bounds) = bounds_cache.borrow().get(&path).copied() else {
                return;
            };
            let active = ActiveDivider {
                split_path: path.clone(),
                divider_idx,
                axis,
                initial_weights: weights.clone(),
                container_origin: bounds.origin,
                container_size: bounds.size,
            };
            panes.update(cx, |p, cx| p.arm_divider(active, cx));
            cx.stop_propagation();
        },
    )
    .into_any_element()
}

/// Full-area capture overlay rendered on top of `ProjectPanes` while a
/// divider is armed. Occluding + topmost so MouseMove only reaches it
/// (not the terminals beneath — no stray events), and it resizes on every
/// move. MouseUp (over it or elsewhere in-window) disarms; an off-window
/// release is caught by the next move's released-button check.
fn divider_capture_overlay(panes: Entity<ProjectPanes>, axis: Axis) -> AnyElement {
    let mut overlay = div().absolute().inset_0().occlude();
    overlay = match axis {
        Axis::Horizontal => overlay.cursor_col_resize(),
        Axis::Vertical => overlay.cursor_row_resize(),
    };
    let move_panes = panes.clone();
    let down_panes = panes.clone();
    let up_panes = panes.clone();
    let up_out_panes = panes.clone();
    overlay
        // The second click of a double-click on a divider lands on THIS
        // overlay (it was raised by the first click's arm), so the divider
        // hitbox never sees it. Handle the reset here too.
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _window, cx| {
            if ev.click_count >= 2 {
                down_panes.update(cx, |p, cx| p.reset_split_double_click(cx));
            }
        })
        .on_mouse_move(move |ev: &MouseMoveEvent, _window, cx| {
            // A move with the primary button no longer down means the
            // release happened off-window; disarm defensively.
            if ev.pressed_button != Some(MouseButton::Left) {
                move_panes.update(cx, |p, cx| p.disarm_divider(cx));
                return;
            }
            move_panes.update(cx, |p, cx| {
                p.resize_active_divider(ev.position, cx);
            });
        })
        .on_mouse_up(
            MouseButton::Left,
            move |_ev: &MouseUpEvent, _window, cx| {
                up_panes.update(cx, |p, cx| p.disarm_divider(cx));
            },
        )
        .on_mouse_up_out(
            MouseButton::Left,
            move |_ev: &MouseUpEvent, _window, cx| {
                up_out_panes.update(cx, |p, cx| p.disarm_divider(cx));
            },
        )
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
            // Prevent, don't explain: a pinned source tab can't be torn
            // out (`take_tab` refuses it), so a split would silently roll
            // back. Suppress the split overlay entirely for a pinned
            // source rather than show an affordance that can't land.
            if payload.source_pinned {
                hover_panes.update(cx, |p, cx| p.set_hovered_drop_target(None, cx));
                return;
            }
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
        .when_some(active_zone, |s, zone| {
            s.child(zone_overlay(group_id, zone, theme))
        })
        .into_any_element()
}

/// Target opacity of the active drop-zone overlay (animated up to this).
const ZONE_OVERLAY_OPACITY: f32 = 0.18;

/// Stable slug per zone — keys the cross-fade animation id so it restarts
/// only on a real zone change, not on every `on_drag_move` frame.
fn zone_slug(zone: Zone) -> &'static str {
    match zone {
        Zone::Center => "center",
        Zone::Left => "left",
        Zone::Right => "right",
        Zone::Up => "up",
        Zone::Down => "down",
    }
}

/// Semi-transparent overlay rectangle for the active drop zone.
/// Sized per zone — half-width / half-height for edge zones, full body
/// for center merge. Positioned absolutely inside the body container.
/// Cross-fades in (~80ms) on each zone change instead of hard-snapping;
/// the animation id is keyed on (group, zone) so it restarts only when the
/// zone actually changes and otherwise settles at full opacity.
fn zone_overlay(group_id: PaneGroupId, zone: Zone, theme: oximux_settings::Theme) -> AnyElement {
    let base = div()
        .absolute()
        .bg(theme.focus_ring)
        .border_2()
        .border_color(theme.focus_ring);
    let base = match zone {
        Zone::Center => base.inset_0(),
        Zone::Left => base.top_0().left_0().h_full().w(gpui::relative(0.5)),
        Zone::Right => base.top_0().right_0().h_full().w(gpui::relative(0.5)),
        Zone::Up => base.top_0().left_0().w_full().h(gpui::relative(0.5)),
        Zone::Down => base.bottom_0().left_0().w_full().h(gpui::relative(0.5)),
    };
    base.with_animation(
        ElementId::Name(format!("zone-overlay-{}-{}", group_id.0, zone_slug(zone)).into()),
        Animation::new(Duration::from_millis(80)),
        |el, delta| el.opacity(ZONE_OVERLAY_OPACITY * delta),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rim_flash_bumps_only_on_cross_group_move_while_split() {
        let a = PaneGroupId(0);
        let b = PaneGroupId(1);
        // First focus (no prior): never flash, even though "split".
        assert_eq!(advance_rim_flash(None, a, true, 0), 0);
        // Same group re-rendered: no bump.
        assert_eq!(advance_rim_flash(Some(a), a, true, 5), 5);
        // Focus moved a -> b while split: bump.
        assert_eq!(advance_rim_flash(Some(a), b, true, 5), 6);
        // Focus moved but NOT split (single group): no bump.
        assert_eq!(advance_rim_flash(Some(a), b, false, 5), 5);
    }

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
