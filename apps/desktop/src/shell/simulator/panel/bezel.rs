//! The device outline every panel state renders into: a light rim, a black
//! bezel, the rounded screen and (phones) the side buttons.
//!
//! The outline is sized from the **measured** space it gets: a canvas records
//! that area each prepaint, and [`fit`] turns it plus the device's display
//! size into exact pixels. That keeps the screen at the stream's own aspect
//! (no letterbox bars inside the bezel) and reflows it for landscape and
//! tablets, which a fixed aspect ratio cannot do in a narrow panel. A changed
//! measurement re-renders the panel once, on the next effect flush.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{AnyElement, Div, IntoElement, ParentElement as _, Styled as _, WeakEntity, canvas, div, px};
use oximux_settings::Theme;
use oximux_simulator::geometry::portrait_to_display;
use oximux_simulator::{DeviceKind, Orientation};

use super::SimulatorPanel;

/// Width of the light rim around the bezel.
const RIM: f32 = 2.0;
/// Bezel thickness bounds; within them it is [`BEZEL_FRACTION`] of the
/// device's short side.
const BEZEL_MIN: f32 = 6.0;
const BEZEL_MAX: f32 = 14.0;
const BEZEL_FRACTION: f32 = 0.035;
/// Side-button thickness; buttons sit this far proud of the rim, so the
/// outline keeps that much room on every side.
const BUTTON_W: f32 = 3.0;
/// Screen corner radius as a fraction of the screen's short side.
const PHONE_CORNER: f32 = 0.13;
const TABLET_CORNER: f32 = 0.035;

/// A phone side button in portrait: `(on the left edge, top as a fraction
/// of the height, length fraction)` — action + volume up/down, then power.
const BUTTONS: [(bool, f32, f32); 4] = [(true, 0.17, 0.035), (true, 0.24, 0.065), (true, 0.32, 0.065), (false, 0.28, 0.1)];

/// What the outline depicts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Device {
    /// The screen as displayed (already swapped for landscape), in any unit:
    /// only its aspect matters.
    pub display: (f32, f32),
    pub kind: DeviceKind,
    pub orientation: Orientation,
}

impl Device {
    /// The outline shown before a stream reports a size: an iPhone 17.
    pub(crate) const PLACEHOLDER: Device =
        Device { display: (1206.0, 2622.0), kind: DeviceKind::Phone, orientation: Orientation::Portrait };
}

/// The outline in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Layout {
    pub w: f32,
    pub h: f32,
    pub bezel: f32,
    pub radius: f32,
    pub screen_radius: f32,
}

/// The largest outline for `device` that fits `area` (with room for the side
/// buttons), or `None` when there is no room at all.
pub(crate) fn fit(area: (f32, f32), device: &Device) -> Option<Layout> {
    let (dw, dh) = device.display;
    if dw <= 0.0 || dh <= 0.0 {
        return None;
    }
    let aspect = dw / dh;
    let room = (area.0 - 2.0 * BUTTON_W, area.1 - 2.0 * BUTTON_W);
    // The bezel scales with the device, so size once with the thickest
    // bezel, then again with the one that size calls for.
    let screen_for = |bezel: f32| {
        let chrome = 2.0 * (bezel + RIM);
        let h = (room.1 - chrome).min((room.0 - chrome) / aspect);
        (h > 0.0).then_some((h * aspect, h))
    };
    let (w0, h0) = screen_for(BEZEL_MAX)?;
    let bezel = ((w0.min(h0) + 2.0 * (BEZEL_MAX + RIM)) * BEZEL_FRACTION).clamp(BEZEL_MIN, BEZEL_MAX);
    let (sw, sh) = screen_for(bezel)?;
    let corner = if device.kind == DeviceKind::Tablet { TABLET_CORNER } else { PHONE_CORNER };
    let screen_radius = sw.min(sh) * corner;
    let chrome = 2.0 * (bezel + RIM);
    Some(Layout { w: sw + chrome, h: sh + chrome, bezel, radius: screen_radius + bezel + RIM, screen_radius })
}

/// A side button placed for `orientation`: `(edge, start, length)` with the
/// edge as `0 = left, 1 = top, 2 = right, 3 = bottom` and start/length as
/// fractions along it. The buttons turn with the device.
pub(crate) fn button_on_edge(orientation: Orientation, left: bool, top: f32, len: f32) -> (u8, f32, f32) {
    let x = if left { 0.0 } else { 1.0 };
    let a = portrait_to_display(orientation, (x, f64::from(top)));
    let b = portrait_to_display(orientation, (x, f64::from(top + len)));
    let (a, b) = ((a.0 as f32, a.1 as f32), (b.0 as f32, b.1 as f32));
    let edge = if a.0 == b.0 {
        if a.0 < 0.5 { 0 } else { 2 }
    } else if a.1 < 0.5 {
        1
    } else {
        3
    };
    let (start, end) = if edge == 0 || edge == 2 { (a.1, b.1) } else { (a.0, b.0) };
    (edge, start.min(end), (end - start).abs())
}

/// The outline, centred in the space it is given and as large as fits, with
/// `screen` inside. `area` is the panel's measurement cell (see the module
/// doc); nothing is drawn until the first measurement lands.
pub(super) fn phone(
    theme: Theme,
    device: &Device,
    area: &Rc<Cell<Option<(f32, f32)>>>,
    panel: WeakEntity<SimulatorPanel>,
    screen: impl IntoElement,
) -> AnyElement {
    let measured = area.clone();
    let meter = canvas(
        move |bounds, _window, cx| {
            let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            if measured.replace(Some(size)) != Some(size) {
                // Too late to affect this frame (and a notify now would be
                // dropped): lay out again once this draw is done.
                cx.defer(move |cx| {
                    let _ = panel.update(cx, |_, cx| cx.notify());
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    let slot = div().relative().flex().flex_1().min_h(px(0.)).w_full().items_center().justify_center().child(meter);
    let Some(layout) = area.get().and_then(|a| fit(a, device)) else {
        return slot.into_any_element();
    };
    slot.child(outline(theme, device, &layout, screen)).into_any_element()
}

fn outline(theme: Theme, device: &Device, layout: &Layout, screen: impl IntoElement) -> Div {
    // Physical hardware: the bezel is black in either theme; the rim takes
    // the theme's muted foreground so it catches light on both grounds.
    let rim = theme.fg_muted;
    let mut outline = div()
        .relative()
        .flex_none()
        .w(px(layout.w))
        .h(px(layout.h))
        .rounded(px(layout.radius))
        .border(px(RIM))
        .border_color(rim)
        .bg(gpui::black())
        .p(px(layout.bezel))
        .child(div().size_full().rounded(px(layout.screen_radius)).overflow_hidden().bg(theme.bg_panel).child(screen));
    if device.kind != DeviceKind::Phone {
        return outline;
    }
    let (w, h) = (layout.w, layout.h);
    let out = -(RIM + BUTTON_W);
    for (left, top, len) in BUTTONS {
        let (edge, start, len) = button_on_edge(device.orientation, left, top, len);
        let button = div().absolute().rounded(px(BUTTON_W)).bg(rim);
        outline = outline.child(match edge {
            0 | 2 => {
                let b = button.top(px(start * h)).h(px(len * h)).w(px(BUTTON_W));
                if edge == 0 { b.left(px(out)) } else { b.right(px(out)) }
            }
            _ => {
                let b = button.left(px(start * w)).w(px(len * w)).h(px(BUTTON_W));
                if edge == 1 { b.top(px(out)) } else { b.bottom(px(out)) }
            }
        });
    }
    outline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(w: f32, h: f32, kind: DeviceKind, orientation: Orientation) -> Device {
        Device { display: (w, h), kind, orientation }
    }

    fn screen_aspect(l: &Layout) -> f32 {
        (l.w - 2.0 * (l.bezel + RIM)) / (l.h - 2.0 * (l.bezel + RIM))
    }

    #[test]
    fn a_portrait_phone_fills_the_height_at_the_screen_aspect() {
        let d = Device::PLACEHOLDER;
        let l = fit((500.0, 900.0), &d).unwrap();
        assert!((l.h - (900.0 - 2.0 * BUTTON_W)).abs() < 0.01, "height-bound: {l:?}");
        assert!(l.w <= 500.0 - 2.0 * BUTTON_W);
        assert!((screen_aspect(&l) - 1206.0 / 2622.0).abs() < 1e-4);
        assert_eq!(l.bezel, BEZEL_MAX);
        assert!(l.radius > l.screen_radius);
    }

    #[test]
    fn a_landscape_phone_is_width_bound_in_a_narrow_panel() {
        let d = device(2622.0, 1206.0, DeviceKind::Phone, Orientation::LandscapeRight);
        let l = fit((460.0, 900.0), &d).unwrap();
        assert!((l.w - (460.0 - 2.0 * BUTTON_W)).abs() < 0.01, "width-bound: {l:?}");
        assert!(l.h < l.w);
        assert!((screen_aspect(&l) - 2622.0 / 1206.0).abs() < 1e-4);
    }

    #[test]
    fn a_small_device_gets_a_thinner_bezel_and_a_tablet_squarer_corners() {
        let small = fit((140.0, 260.0), &Device::PLACEHOLDER).unwrap();
        assert!(small.bezel < BEZEL_MAX && small.bezel >= BEZEL_MIN);
        let tablet = fit((600.0, 900.0), &device(1640.0, 2360.0, DeviceKind::Tablet, Orientation::Portrait)).unwrap();
        let phone = fit((600.0, 900.0), &Device::PLACEHOLDER).unwrap();
        assert!(tablet.screen_radius / tablet.w < phone.screen_radius / phone.w);
    }

    #[test]
    fn no_room_is_no_layout() {
        assert!(fit((20.0, 20.0), &Device::PLACEHOLDER).is_none());
        assert!(fit((400.0, 800.0), &device(0.0, 10.0, DeviceKind::Phone, Orientation::Portrait)).is_none());
    }

    #[test]
    fn buttons_turn_with_the_device() {
        // Portrait: volume on the left, power on the right.
        assert_eq!(button_on_edge(Orientation::Portrait, true, 0.24, 0.065).0, 0);
        assert_eq!(button_on_edge(Orientation::Portrait, false, 0.28, 0.1).0, 2);
        // Turned counter-clockwise, the left edge faces down; clockwise, up.
        assert_eq!(button_on_edge(Orientation::LandscapeLeft, true, 0.24, 0.065).0, 3);
        assert_eq!(button_on_edge(Orientation::LandscapeRight, true, 0.24, 0.065).0, 1);
        let (_, start, len) = button_on_edge(Orientation::LandscapeRight, true, 0.24, 0.065);
        assert!((start - (1.0 - 0.24 - 0.065)).abs() < 1e-5 && (len - 0.065).abs() < 1e-5);
        assert_eq!(button_on_edge(Orientation::PortraitUpsideDown, true, 0.24, 0.065).0, 2);
    }
}
