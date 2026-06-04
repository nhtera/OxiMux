//! Shared animated loading spinner.
//!
//! A continuously rotating circular-arc glyph, single-sourced so every
//! "loading" affordance across the shell (search results, diff view,
//! source-control panel) spins identically instead of each reimplementing
//! the rotation animation (or worse, showing a static icon).

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Hsla, IntoElement, Styled as _, Transformation, ease_in_out,
    percentage,
};
use gpui_component::{Icon, Sizable as _, Size};

/// Continuously rotating loader glyph tinted `color`, at the default
/// extra-small size. Convenience wrapper over [`spinning_loader_sized`] for
/// inline affordances (toolbar slots, single-line status rows).
pub fn spinning_loader(id: &'static str, color: Hsla) -> impl IntoElement {
    spinning_loader_sized(id, color, Size::XSmall)
}

/// Continuously rotating loader glyph tinted `color` at `size`.
///
/// Returns just the animated icon — wrap it in a sized, centered container
/// at the call site so each surface controls its own padding and label.
/// `id` must be unique among sibling elements in the same view so the
/// animation state doesn't alias; a distinct `&'static str` per call site
/// is sufficient.
pub fn spinning_loader_sized(id: &'static str, color: Hsla, size: Size) -> impl IntoElement {
    Icon::default()
        .path("icons/loader-circle.svg")
        .with_size(size)
        .text_color(color)
        .with_animation(
            id,
            // 0.9 s per revolution with an ease-in-out curve — matches the
            // search panel's cadence so spinners on different surfaces stay
            // visually in sync.
            Animation::new(Duration::from_secs_f32(0.9))
                .repeat()
                .with_easing(ease_in_out),
            |this, delta| this.transform(Transformation::rotate(percentage(delta))),
        )
}
