//! Drag plumbing for tab chips — payload type + floating preview view.
//!
//! The chip's `.on_drag` hands a `TabDragPayload` to GPUI; matching
//! `.on_drag_move::<TabDragPayload>` / `.on_drop::<TabDragPayload>`
//! handlers on the same chip (and, in Phase D, on other groups' bodies)
//! consume it. Cross-group drag lives in Phase D — for now the drop
//! handler asserts the source group equals the dropping group.

use gpui::{
    Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px,
};
use oximux_settings::Theme;

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
    /// (Phase D) recompute via `source_tab_idx` instead.
    pub source_visible_idx: usize,
}

/// Floating chip painted under the cursor while dragging. Minimal
/// styling — just enough that the user can see what they're moving.
pub struct TabDragPreview {
    label: SharedString,
    theme: Theme,
}

impl TabDragPreview {
    pub fn new(label: SharedString, theme: Theme) -> Self {
        Self { label, theme }
    }
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(28.0))
            .px(px(12.0))
            .rounded(px(4.0))
            .bg(self.theme.bg_overlay)
            .border_1()
            .border_color(self.theme.border_active)
            .text_size(px(11.0))
            .text_color(self.theme.fg_base)
            .shadow_md()
            .child(self.label.clone())
    }
}
