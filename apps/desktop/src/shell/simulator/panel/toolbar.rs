//! The floating toolbar pill under the phone (the reference layout): Home,
//! annotate, screenshot, record, logs | rotate, power, detach. Controls that
//! act on a device are enabled only while it streams; those whose features
//! land in P7 (annotate, screenshot, record, logs) show, disabled, where they
//! will live.

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div, px};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::Button as HwButton;

use super::SimulatorPanel;
use crate::shell::simulator::state::PanelState;

/// What a toolbar button does.
#[derive(Clone, Copy)]
enum Act {
    Home,
    Rotate,
    Power,
    Detach,
    /// Arrives in P7.
    Later,
}

/// `(id, icon, tooltip, action)`, left to right; `None` draws a divider.
const ITEMS: [Option<(&str, &str, &str, Act)>; 10] = [
    Some(("sim-tb-home", "icons/house.svg", "Home", Act::Home)),
    None,
    Some(("sim-tb-annotate", "icons/pencil.svg", "Annotate (coming soon)", Act::Later)),
    Some(("sim-tb-shot", "icons/camera.svg", "Screenshot (coming soon)", Act::Later)),
    Some(("sim-tb-record", "icons/video.svg", "Record (coming soon)", Act::Later)),
    Some(("sim-tb-logs", "icons/square-terminal.svg", "Logs (coming soon)", Act::Later)),
    None,
    Some(("sim-tb-rotate", "icons/rotate-cw.svg", "Rotate", Act::Rotate)),
    Some(("sim-tb-power", "icons/power.svg", "Shut down simulator", Act::Power)),
    Some(("sim-tb-detach", "icons/log-out.svg", "Detach", Act::Detach)),
];

impl SimulatorPanel {
    pub(super) fn render_toolbar(&self, state: &PanelState, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density) = (self.theme, self.density);
        let live = matches!(state, PanelState::Streaming);
        let attached = self.device(cx).is_some();
        let mut pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .flex_none()
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .rounded_full()
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel);
        for item in ITEMS {
            let Some((id, icon, tip, act)) = item else {
                pill = pill.child(div().w(px(1.0)).h(px(density.h_row * 0.6)).bg(theme.border_inactive));
                continue;
            };
            let enabled = match act {
                Act::Home | Act::Rotate | Act::Power => live,
                Act::Detach => attached,
                Act::Later => false,
            };
            pill = pill.child(
                Button::new(id)
                    .ghost()
                    .small()
                    .icon(Icon::default().path(icon))
                    .tooltip(tip)
                    .disabled(!enabled)
                    .on_click(cx.listener(move |this, _, _window: &mut Window, cx| this.run_toolbar(act, cx))),
            );
        }
        pill.into_any_element()
    }

    fn run_toolbar(&mut self, act: Act, cx: &mut Context<Self>) {
        if let Act::Detach = act {
            return self.detach(cx);
        }
        let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) else { return };
        hub.update(cx, |hub, cx| match act {
            Act::Home => hub.press_button(&udid, HwButton::Home),
            Act::Rotate => hub.rotate(&udid, cx),
            Act::Power => hub.shutdown_device(&udid, cx),
            Act::Detach | Act::Later => {}
        });
    }
}

