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

use gpui::{
    AnyElement, Context, ElementId, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, Styled, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;

/// A clickable value chip. Clicking fires `on_click` with a fresh
/// `&mut SettingsModal`, so handlers read live state rather than a
/// render-time snapshot.
pub(super) fn value_chip(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_click: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    cx: &mut Context<SettingsModal>,
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

/// An iOS-style pill toggle. The track tints to the accent when `value` is
/// on and the knob slides to the corresponding edge. Clicking fires
/// `on_click` with a live `&mut SettingsModal`.
pub(super) fn toggle_switch(
    id: impl Into<ElementId>,
    value: bool,
    theme: Theme,
    on_click: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    cx: &mut Context<SettingsModal>,
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
