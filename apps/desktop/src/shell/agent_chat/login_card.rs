//! Pure (cx-free) renderer for the inline "agent is signed out" banner.
//!
//! Some agent CLIs answer a prompt with a plain "Not logged in · Please run
//! /login" reply instead of a hard error — which otherwise settles as an
//! ordinary assistant turn, leaving the user at a dead end with no hint that
//! the fix is a terminal sign-in. This card sits at the tail of the scroll
//! content (where the error/summary cards live) and turns that state into an
//! action: a caller-supplied control that opens a terminal running the agent's
//! CLI, where `/login` works. The control is passed in because its click needs
//! a `Context` listener the view owns; this stays pure.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

/// Build the inline signed-out banner: a warn-tinted panel naming the provider
/// plus a caller-supplied `action` control ("Open terminal to sign in").
pub(super) fn login_card(
    provider: &str,
    theme: Theme,
    typo: &Typography,
    density: Density,
    action: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .w_full()
        // Same wrap trap as the markdown bodies: without `min_w_0` a flex
        // ancestor honors the longest unwrapped line and overflows the column.
        .min_w_0()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.status_warning.opacity(0.4))
        .bg(theme.status_warning.opacity(0.08))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.status_warning)
                .child(SharedString::from(format!("{provider} appears signed out"))),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(
                    "Run /login in a terminal, then retry the turn.",
                )),
        )
        .child(action)
        .into_any_element()
}
