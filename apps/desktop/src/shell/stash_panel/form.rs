//! The shared parts of the stash section's three form modals — push, branch
//! from, rename.
//!
//! # Pieces, not a body builder
//!
//! The obvious extraction was one function taking every field and emitting a
//! whole dialog. It is the wrong shape here and the push form is why: its body
//! carries a dynamic title, a scrollable path list, a checkbox with its own
//! disabled-and-forced logic, and a note that appears only when the selection
//! spans a rename. A single builder would need a parameter for each, in a
//! fixed order, and the first dialog that wanted them in a different order
//! would fork it again.
//!
//! So what is shared is what genuinely *is* shared: the card's chrome and
//! dismissal keys, and the three text styles plus the button row that were
//! copied verbatim into all three. Each dialog still writes its own body, in
//! its own order.
//!
//! # The chrome carries the dismissal keys
//!
//! `Enter` confirms and `Escape` cancels for all three, on the card's
//! bubble-phase `on_key_down` so ordinary typing inside the `Input` is
//! untouched. This only ever fires because each dialog focuses its INPUT on
//! open — without that the card holds no focus and the handler never sees a
//! keystroke, which is exactly the bug all three shipped with (see any of
//! their `new`).

use gpui::{
    App, Div, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    SharedString, Styled, Window, div, px,
};
use gpui_component::{
    Disableable as _,
    button::{Button, ButtonVariants},
};
use oximux_settings::{Density, Theme, Typography};

/// Width shared by all three forms. A single column of prose and one field;
/// wider reads as a settings pane, narrower wraps the disclosure copy badly.
pub(super) const FORM_WIDTH: f32 = 440.0;

/// The card: focus, dismissal keys, size, padding and floating chrome.
///
/// Returns a `Div` the caller fills with `.child(...)` in whatever order its
/// body needs.
pub(super) fn form_card<K>(
    focus_handle: &FocusHandle,
    theme: &Theme,
    density: &Density,
    on_key: K,
) -> Div
where
    K: Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
{
    use crate::ui::FloatingSurface;
    div()
        .track_focus(focus_handle)
        .on_key_down(on_key)
        .flex()
        .flex_col()
        .w(px(FORM_WIDTH))
        .p(px(density.pad_panel * 2.0))
        .floating_chrome(theme, density)
        .gap(px(density.gap_inline))
}

/// The dialog's heading.
pub(super) fn form_title(
    text: impl Into<SharedString>,
    theme: &Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_md))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child(text.into())
}

/// The line under the title naming what the dialog is acting on.
///
/// `fg_muted`, one step brighter than [`form_note`]'s `fg_subtle`, and the
/// distinction is load-bearing: a subtitle identifies the SUBJECT ("From
/// “fix the parser”") and has to be readable at a glance, while a note
/// explains the CONSEQUENCE and should recede. Flattening the two was caught
/// in review of this very extraction.
pub(super) fn form_subtitle(
    text: impl Into<SharedString>,
    theme: &Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .child(text.into())
}

/// The small caps label above a field.
pub(super) fn form_field_label(
    text: impl Into<SharedString>,
    theme: &Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .text_size(px(typography.t_label_caps))
        .text_color(theme.fg_subtle)
        .child(text.into())
}

/// A paragraph of dimmed explanatory copy — the consumption warning, the
/// rewrite disclosure, the rename note.
pub(super) fn form_note(
    text: impl Into<SharedString>,
    theme: &Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(text.into())
}

/// The trailing Cancel + primary-verb row.
///
/// `confirm_disabled` is passed rather than derived: "empty means disabled" is
/// true for the branch and rename forms and false for push, whose message is
/// genuinely optional. Each caller states its own rule, and the key path in
/// its `try_confirm` has to agree with what it passes here — a button disabled
/// on empty while `Enter` still fires is the shape of bug that ships.
pub(super) fn form_buttons<C, K>(
    cancel_id: &'static str,
    confirm_id: &'static str,
    confirm_label: &'static str,
    confirm_disabled: bool,
    density: &Density,
    on_cancel: C,
    on_confirm: K,
) -> impl IntoElement
where
    C: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    K: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
{
    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap(px(density.gap_inline))
        .child(
            Button::new(cancel_id)
                .ghost()
                .label("Cancel")
                .on_click(on_cancel),
        )
        .child(
            Button::new(confirm_id)
                .primary()
                .label(confirm_label)
                .disabled(confirm_disabled)
                .on_click(on_confirm),
        )
}
