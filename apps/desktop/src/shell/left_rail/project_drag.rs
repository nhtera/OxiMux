//! Drag plumbing for sidebar reorder — payload types, the floating preview
//! chip, and the pure insertion-side helper.
//!
//! The left rail reorders project groups and workspace rows with GPUI's
//! stateless drag idiom: the drag source attaches `.on_drag(payload, …)`,
//! every potential target attaches `.drag_over::<Payload>(…)` to paint an
//! insertion line derived purely from `payload.src_index` vs the target's
//! index, and `.on_drop::<Payload>(…)` computes the destination slot. No
//! shared mutable hover entity — the indicator is a function of the payload
//! and the hovered element alone, so there is no cross-frame state to clear.

use std::rc::Rc;

use gpui::{
    App, BoxShadow, Context, Hsla, IntoElement, ParentElement, Render, SharedString,
    StyleRefinement, Styled, Window, div, point, px,
};

/// Payload attached to an in-flight project-header drag. `src_index` is the
/// header's position in the ordered project list at drag start; the drop
/// handler recomputes the move from the live list by `project_id`, so a
/// concurrent reorder cannot misplace it.
#[derive(Clone, Debug)]
pub struct ProjectDragPayload {
    pub project_id: String,
    pub src_index: usize,
}

/// Payload attached to an in-flight workspace-row drag. Carries `project_id`
/// so a drop handler can reject a cross-group drop (reorder is within-group
/// only). `src_index` is the row's position among its group's rendered rows.
#[derive(Clone, Debug)]
pub struct WorkspaceDragPayload {
    pub workspace_id: String,
    pub project_id: String,
    pub src_index: usize,
}

/// Which edge of the hovered target the insertion line is painted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionSide {
    Top,
    Bottom,
}

/// Decide which edge of the hovered target to mark, given where the dragged
/// item started. An item dragged from above (`src < this`) lands *below* the
/// hovered target, so the line goes on the bottom edge; an item from below
/// lands *above*, so the line goes on the top edge. Dropping onto the source
/// row itself is a no-op and is handled by the caller (no line drawn).
pub fn insertion_side(src_index: usize, this_index: usize) -> InsertionSide {
    if src_index < this_index {
        InsertionSide::Bottom
    } else {
        InsertionSide::Top
    }
}

/// Paint the drop indicator onto a `drag_over` style refinement: a solid 2px
/// accent line inset from the rail's horizontal edges, on the side returned by
/// [`insertion_side`]. Inset is achieved with a matching negative-free design
/// — the border sits on the padded container so it already clears the rail
/// edge by the container's horizontal padding. Shared by the project-header
/// and workspace-row indicators so both read identically.
///
/// A 1px `knockout`-colored casing (a hard, blur-free shadow offset just past
/// the accent edge) hugs the line so it stays legible over a tinted card — the
/// caret reads as "punched through" adjacent content instead of blending into
/// the row color underneath. Pass the rail surface color as `knockout`.
pub fn paint_insertion_line(
    style: StyleRefinement,
    side: InsertionSide,
    accent: gpui::Hsla,
    knockout: gpui::Hsla,
) -> StyleRefinement {
    let (bordered, casing_offset) = match side {
        InsertionSide::Top => (style.border_t(px(2.0)), point(px(0.0), px(-1.0))),
        InsertionSide::Bottom => (style.border_b(px(2.0)), point(px(0.0), px(1.0))),
    };
    bordered.border_color(accent).shadow(vec![BoxShadow {
        color: knockout,
        offset: casing_offset,
        blur_radius: px(0.0),
        spread_radius: px(0.0),
    }])
}

/// Reorder callback: `(moved_workspace_id, target_workspace_id, window, app)`.
/// The rail supplies a closure that calls `WorkspaceRoot::reorder_workspace`.
/// Both ids are real DB rows (the pinned primary is neither draggable nor a
/// drop target), so the repo resolves neighbours by id without a primary-row
/// offset.
pub type WorkspaceReorderFn = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

/// Everything `render_workspace_card` needs to wire drag-to-reorder onto a
/// row. Built by the rail only when the row is reorderable (non-primary, in
/// Manual sort mode); `None` renders a plain, non-draggable card.
#[derive(Clone)]
pub struct WorkspaceDragConfig {
    pub workspace_id: String,
    pub project_id: String,
    /// This row's position among its group's rendered rows — the drop target
    /// index when another row is released onto it.
    pub src_index: usize,
    pub accent: Hsla,
    pub ghost_label: SharedString,
    pub on_reorder: WorkspaceReorderFn,
}

/// Floating chip painted under the cursor while dragging a project header or
/// workspace row. A minimal label pill — enough to read which item is moving
/// without mirroring the full row.
pub struct SidebarDragPreview {
    label: SharedString,
}

impl SidebarDragPreview {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl Render for SidebarDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Every token resolved per render rather than carried in. A ghost that
        // held a palette would be one more place for a stale one to hide, and
        // it has no state worth keeping — it exists for one drag.
        let theme = oximux_settings::appearance::theme(cx);
        let density = oximux_settings::appearance::density(cx);
        let typography = oximux_settings::appearance::typography(cx);
        div()
            .flex()
            .items_center()
            .h(px(24.0))
            .px(px(10.0))
            .rounded(px(density.r_xs))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_base)
            .shadow_md()
            .child(self.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_down_marks_bottom_edge() {
        // src above the target → item lands below it.
        assert_eq!(insertion_side(0, 3), InsertionSide::Bottom);
        assert_eq!(insertion_side(2, 3), InsertionSide::Bottom);
    }

    #[test]
    fn dragging_up_marks_top_edge() {
        // src below the target → item lands above it.
        assert_eq!(insertion_side(5, 3), InsertionSide::Top);
        assert_eq!(insertion_side(4, 3), InsertionSide::Top);
    }

    #[test]
    fn equal_index_defaults_to_top() {
        // Caller suppresses the line on the source row; the bare helper still
        // returns a deterministic side for index equality.
        assert_eq!(insertion_side(3, 3), InsertionSide::Top);
    }
}
