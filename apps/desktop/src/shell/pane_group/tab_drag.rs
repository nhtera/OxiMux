//! Drag plumbing for tab chips — payload type + floating preview view.
//!
//! The chip's `.on_drag` hands a `TabDragPayload` to GPUI; matching
//! `.on_drag_move::<TabDragPayload>` / `.on_drop::<TabDragPayload>`
//! handlers on the same chip (and on other groups' strip bodies) consume
//! it. Same-group drops reorder via `move_tab`; cross-group drops transfer
//! the tab into the destination group, while a pinned source refuses the
//! cross-group move (its destination affordance is suppressed).

use gpui::{
    Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};
use oximux_settings::{Density, Theme};

/// The tab-kind glyph in a drag preview -- the chip's own
/// `ICON_SIZE_PX`, restated here rather than made public because a
/// preview is a separate surface that happens to agree.
const TAB_GLYPH_PX: f32 = 11.0;

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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Resolved rather than cached: the preview exists for the length of one
        // drag, so there is no snapshot to go stale and nothing for
        // `appearance::sync` to refresh. Every measure below is the chip's own,
        // because a preview that does not match what you picked up is worse
        // than no preview.
        let appearance = oximux_settings::appearance::active(cx);
        let density = Density::for_appearance(appearance);
        let typography = oximux_settings::appearance::typography(cx);
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
            .gap(px(density.gap_inline))
            .h(px(density.h_tab))
            .px(px(density.pad_tab))
            .rounded(px(density.r_xs))
            .bg(self.theme.bg_overlay)
            .border_1()
            .border_color(self.theme.border_active)
            .text_size(px(typography.t_body_sm))
            .text_color(self.theme.fg_base)
            .shadow_md()
            .child(
                svg()
                    .size(px(density.scale(TAB_GLYPH_PX)))
                    .path(self.icon_path.clone())
                    .text_color(self.theme.fg_muted),
            )
            .when_some(color_dot, |s, dot| s.child(dot))
            .child(self.label.clone())
    }
}
