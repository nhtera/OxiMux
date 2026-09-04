//! Shared row + chip widgets for the settings modal panes.
//!
//! Editable panes apply immediately: each clickable chip mutates the
//! modal's working copy and persists the TOML in its `on_click`. The
//! file watcher then reloads + repaints, so the panes never push values
//! into globals themselves.
//!
//! Helpers return an owned [`AnyElement`] (rather than `impl IntoElement`)
//! so a pane can build several of them from the same `&mut Context`
//! without tripping Rust 2024's RPIT lifetime-capture rules.
//!
//! Two of them — [`value_chip`] and [`toggle_switch`] — outgrew this modal and
//! are generic over their hosting view, so the Automations page arms a
//! schedule with the identical switch the Schedules pane does. The rest stay
//! modal-bound until something else needs them.

use gpui::{
    AnyElement, Context, ElementId, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};
use gpui_component::tooltip::Tooltip;
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;

/// A clickable value chip. Clicking fires `on_click` with a fresh `&mut V`,
/// so handlers read live state rather than a render-time snapshot.
///
/// Generic over the hosting view rather than pinned to [`SettingsModal`]: the
/// Automations page renders the same chips against the same store, and two
/// chips that merely *look* alike drift apart the first time either is
/// restyled. Call sites in this module infer `V = SettingsModal` unchanged.
pub(crate) fn value_chip<V: 'static>(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_click: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_overlay_item))
        .px(px(10.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(|s| s.border_color(theme.border_active))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, window, cx| on_click(this, window, cx)),
        )
        .child(text.into())
        .into_any_element()
}

/// A square icon-only button: an SVG glyph in a bordered tile, with `tip` as
/// its tooltip. `danger` tints the glyph to the error colour on hover, for an
/// action that destroys something.
///
/// Icon-only because the profile list carries three of these per row and three
/// text chips would leave the profile's own name and résumé no width to wrap
/// into — the exact squeeze this card has failed twice. The tooltip is not
/// decoration: it is the only place the action is named.
#[allow(clippy::too_many_arguments)]
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    tip: impl Into<SharedString>,
    danger: bool,
    theme: Theme,
    density: Density,
    on_click: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let tip = tip.into();
    // A destructive action reads through the tile, not the glyph: `svg()` does
    // not inherit `text_color` from its parent (it paints transparent, which is
    // how this button first shipped as an empty box), and a colour set on the
    // svg itself cannot follow the parent's hover.
    let hover_border = if danger { theme.status_error } else { theme.border_active };
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .size(px(density.h_overlay_item))
        .rounded(px(density.r_chip))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel_alt)
        .cursor_pointer()
        .hover(|s| {
            s.border_color(hover_border).bg(Hsla { a: 0.10, ..hover_border })
        })
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, window, cx| on_click(this, window, cx)),
        )
        .child(svg().path(icon).size(px(14.0)).flex_none().text_color(theme.fg_muted))
        .into_any_element()
}

/// A two-state value chip that tints to the accent when `active`. Reads with
/// colour (info-blue fill + text + border) when on and as a muted resting chip
/// when off, so an enabled flag is legible at a glance instead of relying on an
/// "On"/"Off" word swap alone. Clicking fires `on_click` with a live modal.
#[allow(clippy::too_many_arguments)]
pub(super) fn toggle_chip(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    active: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_click: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let accent_fill = Hsla { a: 0.14, ..theme.status_info };
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_overlay_item))
        .px(px(10.0))
        .rounded(px(density.r_chip))
        .border_1()
        .text_size(px(typography.t_body_sm))
        .cursor_pointer()
        .when(active, |s| {
            s.bg(accent_fill)
                .border_color(theme.status_info)
                .text_color(theme.status_info)
        })
        .when(!active, |s| {
            s.bg(theme.bg_panel_alt)
                .border_color(theme.border_inactive)
                .text_color(theme.fg_muted)
                .hover(|h| h.border_color(theme.border_active).text_color(theme.fg_base))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, window, cx| on_click(this, window, cx)),
        )
        .child(text.into())
        .into_any_element()
}

/// An iOS-style pill toggle. The track tints to the accent when `value` is
/// on and the knob slides to the corresponding edge. Clicking fires
/// `on_click` with a live `&mut V`. Generic for the same reason as
/// [`value_chip`] — the Automations page arms schedules with this switch.
pub(crate) fn toggle_switch<V: 'static>(
    id: impl Into<ElementId>,
    value: bool,
    theme: Theme,
    on_click: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    const TRACK_W: f32 = 36.0;
    const TRACK_H: f32 = 20.0;
    const KNOB: f32 = 14.0;
    const PAD: f32 = 3.0;

    let knob = div().size(px(KNOB)).rounded_full().bg(theme.fg_base);

    let mut track = div()
        .id(id.into())
        .flex()
        .items_center()
        .w(px(TRACK_W))
        .h(px(TRACK_H))
        .rounded(px(TRACK_H / 2.0))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, window, cx| on_click(this, window, cx)),
        );

    // On: accent track, knob pinned right. Off: muted track + outline, knob
    // pinned left.
    track = if value {
        track.bg(theme.status_info).justify_end().pr(px(PAD))
    } else {
        track
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(theme.border_inactive)
            .justify_start()
            .pl(px(PAD))
    };

    track.child(knob).into_any_element()
}

/// A `[−] value [+]` numeric stepper. `on_dec` / `on_inc` mutate + persist.
#[allow(clippy::too_many_arguments)]
pub(super) fn stepper(
    id_prefix: &str,
    value_text: impl Into<SharedString>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_dec: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    on_inc: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let dec = value_chip(
        SharedString::from(format!("{id_prefix}-dec")),
        "−",
        theme,
        density,
        typography,
        on_dec,
        cx,
    );
    let inc = value_chip(
        SharedString::from(format!("{id_prefix}-inc")),
        "+",
        theme,
        density,
        typography,
        on_inc,
        cx,
    );
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(dec)
        .child(
            div()
                .w(px(72.0))
                .flex()
                .justify_center()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(value_text.into()),
        )
        .child(inc)
        .into_any_element()
}

/// A read-only `label: value` row used by the non-editable panes.
pub(super) fn info_row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .w_full()
        .py(px(6.0))
        .child(
            div()
                .w(px(180.0))
                .flex_none()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(label.into()),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(value.into()),
        )
        .into_any_element()
}
