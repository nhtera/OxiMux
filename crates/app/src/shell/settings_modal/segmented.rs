//! Inline segmented picker for the settings panes — a connected row of
//! options where the active one is raised. Replaces the cycling value-chip
//! for small closed enums (bell style, AI mode, agent) so every choice is
//! visible at once. Each segment carries its own apply-and-persist handler.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;

type SegmentHandler = dyn Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>);

/// One option in a [`segmented`] control.
pub(super) struct Segment {
    pub label: SharedString,
    pub selected: bool,
    pub on_select: Box<SegmentHandler>,
}

impl Segment {
    pub(super) fn new(
        label: impl Into<SharedString>,
        selected: bool,
        on_select: impl Fn(&mut SettingsModal, &mut Window, &mut Context<SettingsModal>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            selected,
            on_select: Box::new(on_select),
        }
    }
}

/// Render `segments` as a connected pill track. The selected segment is
/// raised (overlay fill + base text); the rest are muted and brighten on
/// hover. `id_prefix` disambiguates the per-segment element ids.
pub(super) fn segmented(
    id_prefix: &str,
    segments: Vec<Segment>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let mut track = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive);

    for (idx, seg) in segments.into_iter().enumerate() {
        let selected = seg.selected;
        let on_select = seg.on_select;
        track = track.child(
            div()
                .id(SharedString::from(format!("{id_prefix}-{idx}")))
                .flex()
                .items_center()
                .justify_center()
                .h(px(density.h_overlay_item - 6.0))
                .px(px(10.0))
                .rounded(px(density.r_xs))
                .text_size(px(typography.t_body_sm))
                .cursor_pointer()
                .when(selected, |s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                .when(!selected, |s| {
                    s.text_color(theme.fg_muted)
                        .hover(|h| h.text_color(theme.fg_base))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, window, cx| on_select(this, window, cx)),
                )
                .child(seg.label),
        );
    }

    track.into_any_element()
}
