//! VS Code-style search overlay layout.
//!
//! Pure layout function. The host (`TerminalView`) constructs a [`Params`]
//! struct holding the data slice (`query`, `badge`, `options`) plus six
//! `cx.listener`-wrapped click handlers (three toggles + prev/next/close).
//! The overlay file knows nothing about `TerminalView` internals, which
//! keeps the `pub` surface of the view minimal — same host-owns-I/O pattern
//! as `terminal_search_state.rs`.
//!
//! Visual style mirrors VS Code's find widget: flat text toggles with a
//! thin underline for the active state (no fill, no chunky pill). The
//! chevron + close use `gpui_component::Button` for icon-glyph parity with
//! the rest of the UI.
//!
//! Anti-`.occlude()` lesson still applies: the outer container has no
//! `.id()` / `.occlude()` / wrapper listeners. Click capture happens only
//! on the inline toggle divs + nav buttons, so clicks outside their
//! bounding boxes pass through to the terminal grid behind.

use gpui::{
    App, ClickEvent, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    IconName, Sizable as _,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use oximux_settings::{Theme, Typography};

use crate::shell::terminal_search::SearchOptions;

/// Boxed click handler for the nav buttons (prev / next / close). These use
/// `gpui_component::Button`, whose `on_click` callback signature is
/// `Fn(&ClickEvent, ...)`.
pub type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Boxed mouse-down handler for the inline text toggles (Aa / ab / .*).
/// Plain divs don't get `gpui-component`'s click synthesis, so we listen on
/// `on_mouse_down` directly. Boxed for the same reason as `ClickHandler` —
/// keeps the `Params` struct non-generic.
pub type ToggleHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

/// Bundle of inputs for [`build`]. Construction lives at the call site
/// (see `TerminalView::render`); the overlay just consumes it.
pub struct Params<'a> {
    pub query: &'a str,
    pub badge: String,
    pub caret_on: bool,
    pub options: SearchOptions,
    pub theme: &'a Theme,
    pub typography: &'a Typography,
    pub on_toggle_case: ToggleHandler,
    pub on_toggle_word: ToggleHandler,
    pub on_toggle_regex: ToggleHandler,
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
    let toggle_text_size = px(typography.t_body_md);
    let mono = typography.mono_font();

    div()
        .absolute()
        .top(px(8.0))
        .right(px(12.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .px(px(4.0))
        .py(px(3.0))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(6.0))
        .child(
            // Input frame: query | toggles | badge, all on one row. Border
            // uses `focus_ring` because the overlay is the active keyboard
            // target while open (TerminalView intercepts keystrokes through
            // the search state machine), so the input is, in effect,
            // focused — committing to the focused style up front is honest.
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(6.0))
                .py(px(1.0))
                .min_w(px(260.0))
                .bg(theme.bg_base)
                .border_1()
                .border_color(theme.focus_ring)
                .rounded(px(4.0))
                .font(mono.clone())
                .text_size(px(typography.t_body_lg))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(if query_empty {
                            theme.fg_subtle
                        } else {
                            theme.fg_base
                        })
                        .when(query_empty, |this| this.italic())
                        .child(query_text)
                        .child(
                            // VS Code-style caret. `caret_on` syncs with
                            // the terminal's 530ms blink_task — no second
                            // timer. Zero-width when off so layout doesn't
                            // twitch every 530ms.
                            div()
                                .ml(px(2.0))
                                .w(if caret_on { px(2.0) } else { px(0.0) })
                                .h(caret_height)
                                .bg(theme.focus_ring),
                        ),
                )
                // Three flat toggles wrapped in their own tight-gap flex so
                // they cluster as a single visual unit, set apart from the
                // query block (left) and the count badge (right) by the
                // outer frame's wider gap. Active state = thin underline;
                // no fill, no border box — the underline alone reads as
                // "armed" without competing for visual weight against the
                // query and the count badge.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .child(toggle_text(
                            "oximux-search-toggle-case",
                            "Aa",
                            "Match Case",
                            options.case_sensitive,
                            theme,
                            toggle_text_size,
                            on_toggle_case,
                        ))
                        .child(toggle_text(
                            "oximux-search-toggle-word",
                            "ab",
                            "Match Whole Word",
                            options.whole_word,
                            theme,
                            toggle_text_size,
                            on_toggle_word,
                        ))
                        .child(toggle_text(
                            "oximux-search-toggle-regex",
                            ".*",
                            "Use Regular Expression",
                            options.regex,
                            theme,
                            toggle_text_size,
                            on_toggle_regex,
                        )),
                )
                .child(
                    div()
                        .text_color(theme.fg_muted)
                        .text_size(px(typography.t_label_xs))
                        .child(SharedString::from(badge)),
                ),
        )
        .child(
            Button::new("oximux-search-prev")
                .ghost()
                .xsmall()
                .icon(IconName::ChevronUp)
                .tooltip("Previous match (Shift+Enter)")
                .on_click(on_prev),
        )
        .child(
            Button::new("oximux-search-next")
                .ghost()
                .xsmall()
                .icon(IconName::ChevronDown)
                .tooltip("Next match (Enter)")
                .on_click(on_next),
        )
        .child(
            Button::new("oximux-search-close")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip("Close (Esc)")
                .on_click(on_close),
        )
}

/// One flat VS Code-style toggle. Text + (optional) 1 px underline. No
/// padding box, no background — the underline alone signals "armed". Hover
/// lifts text from `fg_muted` to `fg_base` so the toggle still feels
/// interactive without the visual weight of a button. `tooltip` shows on
/// hover via gpui-component's managed Tooltip (same widget the chevron
/// buttons use), so all six controls have visually consistent tooltips.
fn toggle_text(
    id: &'static str,
    label: &'static str,
    tooltip: &'static str,
    active: bool,
    theme: &Theme,
    text_size: gpui::Pixels,
    on_click: ToggleHandler,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(18.0))
        .px(px(2.0))
        .text_size(text_size)
        .text_color(if active {
            theme.fg_base
        } else {
            theme.fg_muted
        })
        .when(active, |this| {
            // 1 px underline beneath the glyph row. `pb(1)` lifts the line
            // a hair off the descenders.
            this.border_b_1().border_color(theme.fg_base).pb(px(1.0))
        })
        .when(!active, |this| this.pb(px(1.0)))
        .cursor_pointer()
        .hover(|s| s.text_color(theme.fg_base))
        .child(label)
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_mouse_down(MouseButton::Left, on_click)
}
