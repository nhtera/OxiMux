//! "What's New" popover — release notes for the staged update.
//!
//! Opens from the title-bar Update pill (see `top_bar::update_pill`). Shows
//! the staged release's notes (the GitHub release body, rendered as
//! markdown) with the restart button, so the user can see what they get
//! before deciding to restart — or close it and let the update apply at the
//! next ordinary quit.
//!
//! The card only ever renders while the updater is `Ready`: the pill is its
//! sole trigger and the pill itself is gated on a staged version, so there
//! is no empty/loading state to design for.

use gpui::{
    AnyElement, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{RestartToUpdate, ToggleWhatsNew};

/// Card width — wide enough for typical release-note bullet lines to read
/// without wrapping mid-word, narrow enough to sit under the pill.
const CARD_WIDTH: f32 = 460.0;
/// Notes area height cap; longer bodies scroll inside the card.
const NOTES_MAX_HEIGHT: f32 = 420.0;

/// The popover card. Positioning (anchored under the pill) and the
/// click-outside backdrop belong to the caller — this is just the card.
pub fn view(
    version: &str,
    notes: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let header = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(16.0))
        .pt(px(14.0))
        .pb(px(10.0))
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .text_size(px(typography.t_body_lg))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child("What's in this update"),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(format!("v{version}")),
        );

    let body: AnyElement = if notes.trim().is_empty() {
        div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child("No release notes for this version.")
            .into_any_element()
    } else {
        // Dark-only app: pin the markdown renderer to the dark highlight
        // theme (its default is a light code theme). Same setup as the chat
        // bubbles' markdown.
        let style = TextViewStyle {
            is_dark: true,
            highlight_theme: HighlightTheme::default_dark(),
            ..Default::default()
        };
        div()
            .w_full()
            // Load-bearing: the markdown view reports its longest UNWRAPPED
            // line as min-content width; without this the card would grow
            // past CARD_WIDTH instead of wrapping.
            .min_w_0()
            .text_size(px(typography.t_body_md))
            .child(TextView::markdown("whats-new-notes", notes.to_string()).style(style))
            .into_any_element()
    };

    let notes_area = div()
        .id("whats-new-scroll")
        .flex()
        .flex_col()
        .w_full()
        .max_h(px(NOTES_MAX_HEIGHT))
        .px(px(16.0))
        .py(px(12.0))
        .overflow_y_scroll()
        .child(body);

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(12.0))
        .border_t_1()
        .border_color(theme.border_inactive)
        .child(footer_button(
            "whats-new-later",
            "Later",
            false,
            theme,
            density,
            typography,
        ))
        .child(footer_button(
            "whats-new-restart",
            "Restart to update",
            true,
            theme,
            density,
            typography,
        ));

    div()
        .flex()
        .flex_col()
        .w(px(CARD_WIDTH))
        .rounded(px(density.r_card))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        .shadow_lg()
        .child(header)
        .child(notes_area)
        .child(footer)
        .into_any_element()
}

/// Footer buttons: "Later" (dismiss — the update still applies at the next
/// quit) and the primary "Restart to update". Both dispatch actions so the
/// card stays entity-free; the WorkspaceRoot handlers own the state.
fn footer_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let (bg, fg, hover_bg) = if primary {
        (
            theme.status_info.alpha(0.9),
            theme.fg_base,
            theme.status_info,
        )
    } else {
        (
            theme.bg_panel_alt,
            theme.fg_muted,
            theme.hover_overlay,
        )
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(24.0))
        .px(px(12.0))
        .rounded(px(density.r_chip))
        .bg(bg)
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_medium)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| {
                if primary {
                    window.dispatch_action(Box::new(RestartToUpdate), cx);
                } else {
                    window.dispatch_action(Box::new(ToggleWhatsNew), cx);
                }
            },
        )
        .child(label)
        .into_any_element()
}
