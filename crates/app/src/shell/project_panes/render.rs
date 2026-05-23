//! Render for `ProjectPanes`: recursively walks the group tree and
//! emits a flex layout where Split nodes become divider-separated rows
//! / columns and Leaf nodes become `PaneGroup` entities.
//!
//! Pure view construction — no mutation. Drag-resize between sibling
//! groups is deferred to a later slice (OQ2 in the step-06 plan); for
//! v1 splits render at fixed 50/50.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

use super::ProjectPanes;
use crate::shell::pane_tree::{Axis, PaneGroupId, PaneTree};

/// Visible width of a divider line, in pixels. Matches the in-group
/// divider so the chrome reads as one consistent stripe.
const DIVIDER_THICKNESS_PX: f32 = 1.0;

impl Render for ProjectPanes {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let tree = self.manager().group_tree().clone();
        let body = render_tree(&tree, self, theme);
        div().flex().flex_col().size_full().child(body)
    }
}

fn render_tree(
    node: &PaneTree<PaneGroupId>,
    panes: &ProjectPanes,
    theme: oximux_settings::Theme,
) -> AnyElement {
    match node {
        PaneTree::Leaf(id) => match panes.group(*id) {
            Some(group) => div()
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .overflow_hidden()
                .child(group)
                .into_any_element(),
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
                row = row.child(slot.child(render_tree(child, panes, theme)));
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
