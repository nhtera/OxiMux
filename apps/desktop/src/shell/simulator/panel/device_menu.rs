//! The toolbar's Device menu: appearance, a location (a preset or one typed
//! in), the status bar, and Erase…. The user's own actions need no agent
//! consent; each runs through the same hub calls an agent's verb does
//! (`hub::device_state`). What the device cannot do is not offered.
//!
//! Erase and a typed location ask in the pill itself, as Shut Down does: a
//! plain confirmation, never typing the device's name.

use gpui::{AnyElement, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Styled as _, Window, div, px};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::DeviceId;
use oximux_simulator::caps::StateSet;
use oximux_simulator::state::StateChange;

use super::SimulatorPanel;

/// A question the pill asks instead of showing its buttons.
pub(crate) enum DevicePrompt {
    /// The device it asks about: the panel's device may change meanwhile.
    Erase(DeviceId),
    /// "lat, lon", and why the last try did not parse.
    Location(Entity<InputState>, Option<String>),
}

/// Places a screenshot or a location-based feature is often checked at.
const PRESETS: [(&str, f64, f64); 4] = [
    ("Ho Chi Minh City", 10.7769, 106.7009),
    ("San Francisco", 37.7749, -122.4194),
    ("London", 51.5074, -0.1278),
    ("Tokyo", 35.6762, 139.6503),
];

/// "10.77, 106.70" (a comma or spaces between) as degrees on Earth.
fn parse_lat_lon(text: &str) -> Option<(f64, f64)> {
    let mut parts = text.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty());
    let (lat, lon) = (parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?);
    let on_earth = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon);
    (parts.next().is_none() && on_earth).then_some((lat, lon))
}

impl SimulatorPanel {
    /// The menu button, for a device that takes any state change; `sim`: a
    /// simulator (the only kind that can forget its location).
    pub(super) fn render_device_menu(&self, state: StateSet, sim: bool, live: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if state == StateSet::NONE {
            return None;
        }
        let weak = cx.weak_entity();
        let menu = Button::new("sim-tb-device")
            .ghost()
            .small()
            .tooltip("Device settings")
            .icon(Icon::default().path("icons/smartphone.svg"))
            .disabled(!live)
            .dropdown_menu(move |mut menu, _window, _cx| {
                let item = |label: &str, change: StateChange| {
                    let weak = weak.clone();
                    PopupMenuItem::new(label.to_owned()).on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |panel, cx| panel.apply_device_state(change.clone(), cx));
                    })
                };
                if state.contains(StateSet::APPEARANCE) {
                    menu = menu.label("Appearance");
                    menu = menu.item(item("Light", StateChange::Appearance { dark: false }));
                    menu = menu.item(item("Dark", StateChange::Appearance { dark: true }));
                }
                if state.contains(StateSet::LOCATION) {
                    menu = menu.separator().label("Location");
                    for (name, lat, lon) in PRESETS {
                        menu = menu.item(item(name, StateChange::Location { lat, lon }));
                    }
                    let weak = weak.clone();
                    menu = menu.item(PopupMenuItem::new("Custom…").on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |panel, cx| panel.ask_location(window, cx));
                    }));
                    if sim {
                        menu = menu.item(item("Clear location", StateChange::ClearLocation));
                    }
                }
                if state.contains(StateSet::STATUS_BAR) {
                    menu = menu.separator();
                    menu = menu.item(item("Clean status bar (9:41)", StateChange::StatusBar { clean: true }));
                    menu = menu.item(item("Restore status bar", StateChange::StatusBar { clean: false }));
                }
                if state.contains(StateSet::ERASE) {
                    let weak = weak.clone();
                    menu = menu.separator().item(PopupMenuItem::new("Erase…").on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |panel, cx| {
                            panel.device_prompt = panel.device(cx).map(DevicePrompt::Erase);
                            cx.notify();
                        });
                    }));
                }
                menu
            });
        Some(menu.into_any_element())
    }

    fn apply_device_state(&mut self, change: StateChange, cx: &mut Context<Self>) {
        if let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) {
            hub.update(cx, |hub, cx| hub.apply_state_from_panel(&udid, change, cx));
        }
    }

    fn ask_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("lat, lon (e.g. 10.77, 106.70)"));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.device_prompt = Some(DevicePrompt::Location(input, None));
        cx.notify();
    }

    fn answer_location(&mut self, set: bool, cx: &mut Context<Self>) {
        let Some(DevicePrompt::Location(input, _)) = &self.device_prompt else { return };
        if !set {
            self.device_prompt = None;
        } else {
            let text = input.read(cx).value().to_string();
            match parse_lat_lon(&text) {
                Some((lat, lon)) => {
                    self.device_prompt = None;
                    self.apply_device_state(StateChange::Location { lat, lon }, cx);
                }
                None => {
                    let input = input.clone();
                    self.device_prompt = Some(DevicePrompt::Location(input, Some("latitude, longitude in degrees".into())));
                }
            }
        }
        cx.notify();
    }

    fn answer_erase(&mut self, erase: bool, cx: &mut Context<Self>) {
        let Some(DevicePrompt::Erase(udid)) = self.device_prompt.take() else { return };
        // Only the device the question named.
        if erase && let Some(hub) = self.hub.clone() && self.device(cx).as_ref() == Some(&udid) {
            hub.update(cx, |hub, cx| hub.erase(&udid, cx).detach());
        }
        cx.notify();
    }

    /// The pill's question, while one is open. The pill holds only the
    /// question and its buttons; what more there is to say goes on a line
    /// under it, so the pill stays within a narrow panel.
    pub(super) fn render_device_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let text = |s: String, color| div().px(px(density.gap_inline)).text_size(px(ty.t_body_sm)).text_color(color).child(s);
        let (pill, note) = match self.device_prompt.as_ref()? {
            DevicePrompt::Erase(_) => (
                self.pill()
                    .child(text(format!("Erase {}?", self.device_name(cx)), theme.fg_base))
                    .child(Button::new("sim-erase-cancel").ghost().small().label("Cancel").on_click(cx.listener(|this, _, _, cx| this.answer_erase(false, cx))))
                    .child(Button::new("sim-erase-ok").danger().small().label("Erase").on_click(cx.listener(|this, _, _, cx| this.answer_erase(true, cx)))),
                text("Every app and its data are deleted.".into(), theme.fg_muted),
            ),
            DevicePrompt::Location(input, error) => (
                self.pill()
                    .child(div().w(px(180.)).child(Input::new(input).small()))
                    .child(Button::new("sim-location-cancel").ghost().small().label("Cancel").on_click(cx.listener(|this, _, _, cx| this.answer_location(false, cx))))
                    .child(Button::new("sim-location-set").primary().small().label("Set").on_click(cx.listener(|this, _, _, cx| this.answer_location(true, cx)))),
                match error {
                    Some(error) => text(error.clone(), theme.status_error),
                    None => text("latitude, longitude".into(), theme.fg_muted),
                },
            ),
        };
        Some(div().flex().flex_col().items_center().gap(px(density.gap_inline)).child(pill).child(note).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_location_is_latitude_then_longitude() {
        assert_eq!(parse_lat_lon("10.77, 106.70"), Some((10.77, 106.70)));
        assert_eq!(parse_lat_lon(" -33.86 151.21 "), Some((-33.86, 151.21)));
        for bad in ["", "10.77", "a, b", "91, 0", "0, 181", "1, 2, 3"] {
            assert_eq!(parse_lat_lon(bad), None, "{bad}");
        }
    }
}
