//! Drag plumbing for tab chips — payload type + floating preview view.
//!
//! The chip's `.on_drag` hands a `TabDragPayload` to GPUI; matching
//! `.on_drag_move::<TabDragPayload>` / `.on_drop::<TabDragPayload>`
//! handlers on the same chip (and, in Phase D, on other groups' bodies)
//! consume it. Cross-group drag lives in Phase D — for now the drop
//! handler asserts the source group equals the dropping group.

use gpui::{
    Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};
use oximux_settings::Theme;

use crate::shell::pane_group::TabColor;
use crate::shell::pane_tree::PaneGroupId;

/// Payload attached to an in-flight tab drag. Carries the source group
/// (so cross-group drops in Phase D can resolve the source entity) and
/// the visible position the chip occupied when the drag began.
#[derive(Clone, Debug)]
pub struct TabDragPayload {
    pub source_group: PaneGroupId,
    /// Insertion-order index of the dragged tab — stable across the
    /// visible reorder that drop-time `move_tab` performs.
    pub source_tab_idx: usize,
    /// Visible position at drag start. The drop handler uses this when
    /// the user drops back into the source group; cross-group drops
    /// recompute via `source_tab_idx` instead.
    pub source_visible_idx: usize,
    /// Whether the dragged tab is pinned. Carried in the payload so any
    /// destination handler can suppress the cross-group insertion bar /
    /// split affordance for a pinned source without holding a reference
    /// to the source group (pinned tabs refuse `take_tab`, so the move
    /// would silently fail — prevent the affordance instead of letting
    /// the user aim at an action that can't land).
    pub source_pinned: bool,
}

/// Floating chip painted under the cursor while dragging. Mirrors the
/// source chip's leading content (icon + optional color dot + label) so it
/// reads as the same tab and — since GPUI anchors the preview at
/// `cursor − grab_offset` — the grab point tracks the cursor.
pub struct TabDragPreview {
    label: SharedString,
    icon_path: SharedString,
    color: Option<TabColor>,
    theme: Theme,
}

impl TabDragPreview {
    pub fn new(
        label: SharedString,
        icon_path: SharedString,
        color: Option<TabColor>,
        theme: Theme,
    ) -> Self {
        Self {
            label,
            icon_path,
            color,
            theme,
        }
    }
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Color dot mirrors the chip's color tag, when set.
        let color_dot = self.color.map(|c| {
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(gpui::rgb(c.rgb()))
        });
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .h(px(28.0))
            .px(px(12.0))
            .rounded(px(4.0))
            .bg(self.theme.bg_overlay)
            .border_1()
            .border_color(self.theme.border_active)
            .text_size(px(11.0))
            .text_color(self.theme.fg_base)
            .shadow_md()
            .child(
                svg()
                    .size(px(11.0))
                    .path(self.icon_path.clone())
                    .text_color(self.theme.fg_muted),
            )
            .when_some(color_dot, |s, dot| s.child(dot))
            .child(self.label.clone())
    }
}
