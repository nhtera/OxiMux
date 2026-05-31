//! Scrim + spinner + stop button rendered over the commit textarea
//! while the AI message generator is in flight.
//!
//! Pure render fn — no entity, no state. The caller (`CommitArea`)
//! owns the lifecycle and decides per render whether to mount the
//! overlay based on its own `AiState`. The closure passed as
//! `on_cancel` fires when the Stop button is clicked; the caller
//! is responsible for setting any cancel flag the generation task
//! observes.
//!
//! Layout: an absolutely-positioned div sized to fill its anchor
//! (the textarea's relative wrapper) with a translucent dim layer
//! plus a centered rotating spinner and a small Stop pill. The
//! anchor MUST have `position: relative` (the existing commit-area
//! wrapper does) for the absolute positioning to clip correctly.
//!
//! Why a render fn instead of an `Entity<AiOverlay>` ?: the overlay
//! has no internal state — every prop is derived from the caller's
//! state on each render. An entity would force a `cx.new` + storage
//! field for zero behavior gain. Matches the
//! `conflict_banner::render_*` and `bulk_action_bar` patterns
//! already used in this codebase.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, ClickEvent, IntoElement, ParentElement, Styled, Transformation,
    Window, div, ease_in_out, percentage, px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_settings::Theme;

use crate::shell::source_control::style as sc_style;

/// Sub-px stroke width for the dim scrim, expressed as an alpha
/// channel ratio applied to the existing `bg_panel`. Tuned so the
/// textarea content underneath stays legible (not pure black) while
/// signalling "this surface is busy".
///
/// 0.65 matches the modal-confirm scrim used elsewhere — keeps the
/// "busy / pending input" visual language consistent.
const SCRIM_ALPHA: f32 = 0.65;

/// Spinner half-period in seconds. Same value as the search panel
/// status spinner (`shell/search_panel/header_render.rs`).
const SPINNER_PERIOD: f32 = 0.9;

/// Render the AI generation overlay. Returns `None` when `visible`
/// is false so the caller can `.children(iter)` it directly without
/// an outer `if`. The Stop button fires `on_cancel` when clicked;
/// the caller must own setting the cancel flag the generation task
/// observes.
///
/// The overlay does NOT call `cx.stop_propagation` on the scrim —
/// the textarea underneath is already disabled-input via the
/// `AiState::Generating` gate the caller maintains, so a stray
/// click on the scrim simply lands harmlessly on the disabled
/// input.
pub fn render_ai_overlay<F>(
    visible: bool,
    theme: Theme,
    on_cancel: F,
) -> Option<impl IntoElement>
where
    F: Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
{
    if !visible {
        return None;
    }

    let spinner = Icon::default()
        .path("icons/loader-circle.svg")
        .small()
        .text_color(theme.fg_muted)
        .with_animation(
            "sc-ai-spinner",
            Animation::new(Duration::from_secs_f32(SPINNER_PERIOD))
                .repeat()
                .with_easing(ease_in_out),
            |this, delta| this.transform(Transformation::rotate(percentage(delta))),
        );

    let stop_button = Button::new("sc-ai-stop")
        .ghost()
        .xsmall()
        .label("Stop")
        .icon(
            Icon::default()
                .path("icons/x.svg")
                .size(px(sc_style::ICON)),
        )
        .tooltip("Cancel generation")
        .on_click(on_cancel);

    let mut scrim = theme.bg_panel;
    scrim.a = SCRIM_ALPHA;

    Some(
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(sc_style::PAD_V))
            .bg(scrim)
            .child(spinner)
            .child(stop_button),
    )
}
