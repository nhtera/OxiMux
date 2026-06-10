//! Cross-cutting button-shaped primitives.
//!
//! The gpui-component `Button` family resolves its own `text_color` at
//! render time from the variant's style table, which means a `.ghost()`
//! Button cannot be repainted in `status_error` via `Styled::text_color`
//! — the variant overwrites the refinement. For row-level destructive
//! actions (Drop a stash, Remove a worktree) we need a button that
//! reads quietly inside a row of `.ghost().xsmall()` siblings AND
//! signals danger via a red foreground.
//!
//! `danger_ghost` returns an `AnyElement` shaped like a `Button::ghost`
//! that paints its label + icon in `theme.status_error`. The hover
//! background stays `theme.hover_overlay` (consistent with every other
//! row hover); we do NOT tint the hover red because that conflicts
//! with the focus cursor and reads as "the button broke".
//!
//! Use this whenever the row context already carries `ghost().xsmall()`
//! siblings — full `.danger()` Button drops a saturated red box into the
//! row and ruins the hierarchy. The stash Drop / worktree Remove
//! buttons are the canonical adoption sites.

use gpui::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, Styled, Window, div, px,
};

use oximux_settings::{Density, Theme, Typography};

/// Default item height for a row-level destructive action, matching
/// the `.xsmall()` Button height other row siblings use.
const DANGER_GHOST_H: f32 = 22.0;
const DANGER_GHOST_PAD_X: f32 = 8.0;

/// Render a ghost-styled destructive action sized for inline row use.
///
/// Visual: transparent background, `theme.status_error` foreground,
/// `theme.hover_overlay` hover, `density.r_xs` corner radius. Matches
/// the look of `.ghost().xsmall()` Buttons next to it, but with
/// destructive coloring on label + any embedded icon (via inherited
/// `text_color` on the row's children).
///
/// `on_click` receives the same `(ClickEvent, &mut Window, &mut App)`
/// signature as `Button::on_click`, so adoption sites can swap by
/// changing the constructor without touching their handler.
pub fn danger_ghost<H>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: &Theme,
    density: &Density,
    typography: &Typography,
    on_click: H,
) -> AnyElement
where
    H: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let label: SharedString = label.into();
    let hover_bg = theme.hover_overlay;
    let fg = theme.status_error;

    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(DANGER_GHOST_H))
        .px(px(DANGER_GHOST_PAD_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_sm))
        .text_color(fg)
        .font_weight(typography.w_medium)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window, cx| {
                // Synthesize a ClickEvent from the MouseDown — the caller
                // contract mirrors `Button::on_click`, which fires on
                // mouse-down for snappy feedback (button libs that wait
                // for mouse-up feel sluggish in dense row contexts).
                on_click(&ClickEvent::default(), window, cx);
            },
        )
        .into_any_element()
}
