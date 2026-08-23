//! Pure (cx-free) renderer for the inline turn-error card.
//!
//! When a turn ends in error *after* the transcript is non-empty, the empty-
//! state hint (which only paints on a blank transcript) never surfaces it, so
//! the failure would otherwise be invisible — no card, no way to retry. This
//! card sits at the tail of the scroll content — like the working spinner and
//! usage footer — showing the (capped) error text plus a Retry slot the view
//! fills with a live button (the click needs a `Context` listener, so the view
//! owns it while this stays pure).

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::bubble::elide;

/// Longest error text shown inline before eliding — a runaway stderr dump must
/// not push the composer off-screen.
const MAX_ERROR_CHARS: usize = 600;

/// Build the inline error card: a bordered, error-tinted panel with the failure
/// text and a caller-supplied `retry` control. Pure so `mod.rs` stays small.
pub(super) fn error_card(
    message: &str,
    theme: Theme,
    typo: &Typography,
    density: Density,
    retry: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .w_full()
        // Same wrap trap as the markdown bodies: without `min_w_0` a flex
        // ancestor honors the longest unwrapped line and overflows the column.
        .min_w_0()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.status_error.opacity(0.4))
        .bg(theme.status_error.opacity(0.08))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.status_error)
                .child(SharedString::from("Turn failed")),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(elide(message, MAX_ERROR_CHARS))),
        )
        .child(div().flex().flex_row().justify_end().w_full().child(retry))
        .into_any_element()
}
