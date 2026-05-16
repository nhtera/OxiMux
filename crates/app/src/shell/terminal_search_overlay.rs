//! VS Code-style search overlay layout.
//!
//! Pure layout function. The host (`TerminalView`) constructs a [`Params`]
//! struct holding the data slice (`query`, `badge`, `options`) plus six
//! `cx.listener`-wrapped click handlers (three toggles + prev/next/close).
//! The overlay file knows nothing about `TerminalView` internals, which
//! keeps the `pub` surface of the view minimal — same host-owns-I/O pattern
//! as `terminal_search_state.rs`.
//!
//! Anti-`.occlude()` lesson still applies: the outer container has no
//! `.id()` / `.occlude()` / wrapper listeners. Click capture happens only
//! on the inline `Button` children, so clicks outside their bounding boxes
//! pass through to the terminal grid behind.

use gpui::{
    App, ClickEvent, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants},
};
use oximux_settings::{Theme, Typography};

use crate::shell::terminal_search::SearchOptions;

/// Boxed click handler. Each handler is invoked at most once per render
/// (`Button` takes ownership), so one boxed allocation per overlay build is
/// the cost. Boxing here lets us bundle all six handlers in a non-generic
/// `Params` struct instead of carrying six type parameters through the
/// build function.
pub type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Bundle of inputs for [`build`]. Construction lives at the call site
/// (see `TerminalView::render`); the overlay just consumes it.
pub struct Params<'a> {
    pub query: &'a str,
    pub badge: String,
    pub caret_on: bool,
    pub options: SearchOptions,
    pub theme: &'a Theme,
    pub typography: &'a Typography,
    pub on_toggle_case: ClickHandler,
    pub on_toggle_word: ClickHandler,
    pub on_toggle_regex: ClickHandler,
    pub on_prev: ClickHandler,
    pub on_next: ClickHandler,
    pub on_close: ClickHandler,
}

/// Build the search overlay element. See [`Params`] for inputs.
///
/// The trailing `+ use<>` is required under Rust 2024's precise-capture
/// rules. Without it, the compiler conservatively captures every input
/// lifetime, and the returned element ends up borrowing from `params`'s
/// short-lived `&Theme` / `&Typography` references. With `use<>` we opt
/// into capturing nothing — the returned element owns its data after
/// `build` runs.
pub fn build(params: Params<'_>) -> impl IntoElement + use<> {
    let Params {
        query,
        badge,
        caret_on,
        options,
        theme,
        typography,
        on_toggle_case,
        on_toggle_word,
        on_toggle_regex,
        on_prev,
        on_next,
        on_close,
    } = params;
    let query_empty = query.is_empty();
    let query_text = if query_empty {
        SharedString::from("Find")
    } else {
        SharedString::from(query.to_string())
    };
    // Caret height tracks the body font so the bar visually lines up with
    // glyph baseline. Pad +2 px so it isn't shorter than the tallest glyph.
    let caret_height = px(typography.t_body_lg + 2.0);

    div()
        .absolute()
        .top(px(8.0))
        .right(px(12.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .py(px(4.0))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(6.0))
        .child(
            // Input-styled query box. Border uses `focus_ring` because the
            // overlay is always the active keyboard target while open (the
            // TerminalView intercepts keystrokes through the search state
            // machine), so the input is, in effect, focused — committing
            // to the focused style up front is honest.
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(2.0))
                .min_w(px(220.0))
                .bg(theme.bg_base)
                .border_1()
                .border_color(theme.focus_ring)
                .rounded(px(4.0))
                .font_family(typography.family_mono.clone())
                .text_size(px(typography.t_body_lg))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_1()
                        .text_color(if query_empty {
                            theme.fg_subtle
                        } else {
                            theme.fg_base
                        })
                        .when(query_empty, |this| this.italic())
                        .child(query_text)
                        .child(
                            // VS Code-style caret. `caret_on` lets the host
                            // sync this with the terminal's 530ms blink so
                            // there's no second timer. When off, the caret
                            // is still in DOM with zero width so layout
                            // doesn't twitch every 530ms.
                            div()
                                .ml(px(2.0))
                                .w(if caret_on { px(2.0) } else { px(0.0) })
                                .h(caret_height)
                                .bg(theme.focus_ring),
                        ),
                )
                // Three inline toggles inside the input frame, before the
                // count badge. Order mirrors VS Code: Aa, ab, .*. The
                // active background is the theme `focus_ring` tinted at the
                // standard component-active level via `selected(true)` —
                // gpui-component's Button toggles its own bg when selected.
                .child(toggle_button(
                    "oximux-search-toggle-case",
                    "Aa",
                    "Match case (Aa)",
                    options.case_sensitive,
                    theme,
                    on_toggle_case,
                ))
                .child(toggle_button(
                    "oximux-search-toggle-word",
                    "ab",
                    "Whole word (ab)",
                    options.whole_word,
                    theme,
                    on_toggle_word,
                ))
                .child(toggle_button(
                    "oximux-search-toggle-regex",
                    ".*",
                    "Regex (.*)",
                    options.regex,
                    theme,
                    on_toggle_regex,
                ))
                .child(
                    div()
                        .ml(px(4.0))
                        .text_color(theme.fg_muted)
                        .text_size(px(typography.t_label_xs))
                        .child(SharedString::from(badge)),
                ),
        )
        .child(
            Button::new("oximux-search-prev")
                .ghost()
                .small()
                .icon(IconName::ChevronUp)
                .tooltip("Previous match (Shift+Enter)")
                .on_click(on_prev),
        )
        .child(
            Button::new("oximux-search-next")
                .ghost()
                .small()
                .icon(IconName::ChevronDown)
                .tooltip("Next match (Enter)")
                .on_click(on_next),
        )
        .child(
            Button::new("oximux-search-close")
                .ghost()
                .small()
                .icon(IconName::Close)
                .tooltip("Close (Esc)")
                .on_click(on_close),
        )
}

/// Build one of the three inline toggle buttons. Uses gpui-component's
/// `Button` so the active-state styling stays consistent with the rest of
/// the UI's button surfaces. The active background is derived from
/// `focus_ring` so the toggles read as "armed" without competing with the
/// surrounding chrome.
fn toggle_button(
    id: &'static str,
    label: &'static str,
    tooltip: &'static str,
    active: bool,
    theme: &Theme,
    on_click: ClickHandler,
) -> impl IntoElement {
    let mut btn = Button::new(id)
        .ghost()
        .small()
        .label(label)
        .tooltip(tooltip)
        .on_click(on_click);
    if active {
        btn = btn.selected(true);
    }
    div()
        .when(active, |this| {
            // Backstop styling: in case the host theme's Button "selected"
            // bg is too subtle to read on the dark overlay, this border
            // gives the toggle a visible armed state. Cheap and idempotent
            // — gets overridden by Button's own pressed-state color when
            // gpui-component decides to repaint.
            this.border_1()
                .border_color(theme.focus_ring)
                .rounded(px(4.0))
        })
        .child(btn)
}
