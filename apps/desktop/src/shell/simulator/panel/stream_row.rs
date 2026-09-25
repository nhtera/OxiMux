//! The stream settings row: frame rate, resolution, FPS readout. One store
//! only — `simulator.toml` via [`SimulatorSettings`] — and a change is also
//! applied to the live session at once. (An Encoding control arrives with
//! H.264 in P10; no dead controls before then.)

use gpui::{AnyElement, App, Context, IntoElement, ParentElement as _, SharedString, Styled as _, div, px};
use gpui_component::checkbox::Checkbox;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
};

use super::SimulatorPanel;
use crate::app_settings::simulator_settings::{self, ALLOWED_FPS, Resolution, SimulatorSettings};

/// The current settings (defaults when the global is not installed, e.g. in
/// tests).
pub(crate) fn settings(cx: &App) -> SimulatorSettings {
    cx.try_global::<SimulatorSettings>().cloned().unwrap_or_default()
}

impl SimulatorPanel {
    pub(super) fn render_stream_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let stream = settings(cx).stream;
        let weak = cx.weak_entity();
        let weak_res = weak.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .h(px(density.h_row))
            .px(px(density.pad_panel))
            .text_size(px(ty.t_body_sm))
            .text_color(theme.fg_muted)
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                Button::new("sim-fps")
                    .ghost()
                    .xsmall()
                    .label(SharedString::from(format!("{} fps", stream.fps)))
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for fps in ALLOWED_FPS {
                            let weak = weak.clone();
                            menu = menu.item(
                                PopupMenuItem::new(format!("{fps} fps")).checked(fps == stream.fps).on_click(
                                    move |_, _window, cx| {
                                        let _ = weak.update(cx, |panel, cx| panel.update_stream(cx, |s| s.stream.fps = fps));
                                    },
                                ),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Button::new("sim-resolution")
                    .ghost()
                    .xsmall()
                    .label(match stream.resolution {
                        Resolution::Half => "Half",
                        Resolution::Full => "Full",
                    })
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for (label, res) in [("Half", Resolution::Half), ("Full", Resolution::Full)] {
                            let weak = weak_res.clone();
                            menu = menu.item(PopupMenuItem::new(label).checked(res == stream.resolution).on_click(
                                move |_, _window, cx| {
                                    let _ = weak.update(cx, |panel, cx| panel.update_stream(cx, |s| s.stream.resolution = res));
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(
                Checkbox::new("sim-show-fps")
                    .label("FPS")
                    .checked(stream.show_fps)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        let checked = *checked;
                        this.update_stream(cx, |s| s.stream.show_fps = checked);
                    })),
            )
            .into_any_element()
    }

    /// Change the settings, persist them, and apply rate/scale to the live
    /// session.
    fn update_stream(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut SimulatorSettings)) {
        let mut next = settings(cx);
        change(&mut next);
        let next = next.sanitized();
        if let Err(e) = simulator_settings::save(&next) {
            tracing::warn!("simulator settings not saved: {e}");
        }
        let (scale, fps) = (f64::from(next.stream.effective_scale()), f64::from(next.stream.fps));
        cx.set_global(next);
        if let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) {
            hub.update(cx, |hub, cx| hub.configure_stream(&udid, scale, fps, cx));
        }
        cx.notify();
    }
}
