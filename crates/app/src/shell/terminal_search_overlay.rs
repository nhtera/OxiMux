//! VS Code-style search overlay layout.
//!
//! Pure layout function. The host (`TerminalView`) supplies the data slice
//! (`query`, `badge`) and three `cx.listener`-wrapped closures for the
//! click handlers (prev / next / close). The overlay file knows nothing
//! about `TerminalView` internals, which keeps the `pub` surface of the
//! view minimal — same host-owns-I/O pattern as `terminal_search_state.rs`.
//!
//! Anti-`.occlude()` lesson still applies: the outer container has no
//! `.id()` / `.occlude()` / wrapper listeners. Click capture happens only
//! on the three `Button` children, so clicks outside their bounding boxes
//! pass through to the terminal grid behind.

use gpui::{
    App, ClickEvent, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    IconName, Sizable as _,
    button::{Button, ButtonVariants},
};
use oximux_settings::{Theme, Typography};

/// Build the search overlay element.
///
/// `query` is the live needle. `badge` is the pre-formatted count
/// (e.g. `"3 of 47"`); empty when no query is active. `caret_on` toggles
/// the blue caret on/off — caller threads `TerminalView::cursor_visible`
/// so the overlay caret blinks in sync with the terminal cursor (same
/// 530ms tick, no new timer). The three closures run on click —
/// typically `cx.listener` wrappers that mutate `TerminalView::search`
/// and call `cx.notify()`.
// 8 args is at clippy's threshold but each is load-bearing (3 click
// closures + caret state + theme + typography + query + badge) and
// bundling them into a struct adds boilerplate at every call site
// without simplifying the interface. Single call site, single owner.
#[allow(clippy::too_many_arguments)]
pub fn build<F1, F2, F3>(
    query: &str,
    badge: String,
    caret_on: bool,
    theme: &Theme,
    typography: &Typography,
    on_prev: F1,
    on_next: F2,
    on_close: F3,
) -> impl IntoElement + use<F1, F2, F3>
where
    F1: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F2: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F3: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let query_empty = query.is_empty();
    let query_text = if query_empty {
        SharedString::from("Find")
    } else {
        SharedString::from(query.to_string())
    };
    // Caret: thin vertical bar that signals "ready to type". Static (no
    // blink) — adding a blink timer for an overlay that's already visually
    // active would burn a notify every 530ms while open. The overlay
    // itself disappearing on Esc is the strong "off" signal.
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
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(2.0))
                .min_w(px(180.0))
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
                .child(
                    div()
                        .ml_auto()
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
                .on_click(on_prev),
        )
        .child(
            Button::new("oximux-search-next")
                .ghost()
                .small()
                .icon(IconName::ChevronDown)
                .on_click(on_next),
        )
        .child(
            Button::new("oximux-search-close")
                .ghost()
                .small()
                .icon(IconName::Close)
                .on_click(on_close),
        )
}
