//! Render for `ProjectPanes`: recursively walks the group tree and
//! emits a flex layout where Split nodes become divider-separated rows
//! / columns and Leaf nodes become `PaneGroup` entities.
//!
//! Pure view construction — no mutation. Drag-resize between sibling
//! groups is deferred to a later slice (OQ2 in the step-06 plan); for
//! v1 splits render at fixed 50/50.

use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

use super::ProjectPanes;
use crate::shell::pane_group::render::build_tab_strip_for;
use crate::shell::pane_tree::{Axis, PaneGroupId, PaneTree};

/// Visible width of a divider line, in pixels. Matches the in-group
/// divider so the chrome reads as one consistent stripe.
const DIVIDER_THICKNESS_PX: f32 = 1.0;

impl Render for ProjectPanes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drop any group whose tabs got closed down to zero. Refusing
        // the last group keeps the workspace anchored.
        self.purge_empty_groups(window, cx);

        let theme = self.theme;
        let tree = self.manager().group_tree().clone();
        // The first group in DFS order has its tab strip hoisted into
        // the workspace top bar (see `topmost_tab_strip`), so we skip
        // its inline strip here.
        let topmost = self.manager().in_order_groups().first().copied();
        let body = render_tree(&tree, self, topmost, theme, cx);
        div().flex().flex_col().size_full().child(body)
    }
}

impl ProjectPanes {
    /// Strip for the workspace's topmost (first-in-DFS) pane group,
    /// to be embedded in the top bar's center zone. `None` when no
    /// groups are mounted.
    pub fn topmost_tab_strip(&self, cx: &App) -> Option<AnyElement> {
        let id = self.manager().in_order_groups().first().copied()?;
        let entity = self.group(id)?;
        Some(build_tab_strip_for(entity, self.theme, cx))
    }
}

fn render_tree(
    node: &PaneTree<PaneGroupId>,
    panes: &ProjectPanes,
    topmost: Option<PaneGroupId>,
    theme: oximux_settings::Theme,
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
                let is_topmost = Some(*id) == topmost;
                // Hoist the topmost group's strip into the top bar.
                // Non-topmost groups composed entirely of plain shell
                // terminals also drop the strip — clean split panes
                // with raw terminal content, no per-pane header chip.
                let suppress_inline_strip =
                    is_topmost || group.read(cx).is_terminal_only();
                if suppress_inline_strip {
                    slot.child(group).into_any_element()
                } else {
                    slot.child(build_tab_strip_for(group.clone(), theme, cx))
                        .child(group)
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
                row = row.child(slot.child(render_tree(child, panes, topmost, theme, cx)));
                if i + 1 < children.len() {
                    row = row.child(divider(*axis, theme));
                }
            }
            row.into_any_element()
        }
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
