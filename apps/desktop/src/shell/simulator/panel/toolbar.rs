//! The floating toolbar pill under the phone, in the reference layout:
//! Home | Annotate, Screenshot, Record | Logs | Rotate | Shutdown, Detach —
//! and for Android, Back, Home, Recents in front (the navigation bar's three).
//! Buttons dispatch the same `Sim*` actions as the captured-keyboard
//! shortcuts and the palette (see [`super::commands`]). While a shutdown is
//! being confirmed, the pill becomes the question.

use std::time::Instant;

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use super::{RootRequest, SimulatorPanel};
use super::commands::SimCommand;
use crate::shell::simulator::state::PanelState;

/// `(id, icon, tooltip, command)`, left to right; `None` draws a divider.
/// Tooltips name the shortcut, which works while typing into the simulator.
const ITEMS: [Option<(&str, &str, &str, SimCommand)>; 12] = [
    Some(("sim-tb-home", "icons/house.svg", "Home  ⌘⇧H", SimCommand::Home)),
    None,
    Some(("sim-tb-annotate", "icons/pencil.svg", "Annotate for an agent", SimCommand::Annotate)),
    Some(("sim-tb-shot", "icons/camera.svg", "Screenshot to Desktop  ⌘S", SimCommand::Screenshot)),
    Some(("sim-tb-record", "icons/video.svg", "Record screen  ⌘R", SimCommand::ToggleRecord)),
    None,
    Some(("sim-tb-logs", "icons/square-terminal.svg", "Device logs", SimCommand::OpenLogs)),
    None,
    Some(("sim-tb-rotate", "icons/rotate-cw.svg", "Rotate  ⌘→", SimCommand::RotateCw)),
    None,
    Some(("sim-tb-power", "icons/power.svg", "Shut down simulator", SimCommand::Shutdown)),
    Some(("sim-tb-detach", "icons/log-out.svg", "Detach", SimCommand::Detach)),
];

/// The Android pill: the navigation bar's three buttons, then the rest.
const ANDROID_ITEMS: [Option<(&str, &str, &str, SimCommand)>; 14] = [
    Some(("sim-tb-back", "icons/arrow-left.svg", "Back", SimCommand::Back)),
    Some(("sim-tb-home", "icons/house.svg", "Home  ⌘⇧H", SimCommand::Home)),
    Some(("sim-tb-recents", "icons/square.svg", "Recents", SimCommand::Recents)),
    None,
    Some(("sim-tb-annotate", "icons/pencil.svg", "Annotate for an agent", SimCommand::Annotate)),
    Some(("sim-tb-shot", "icons/camera.svg", "Screenshot to Desktop  ⌘S", SimCommand::Screenshot)),
    Some(("sim-tb-record", "icons/video.svg", "Record screen  ⌘R", SimCommand::ToggleRecord)),
    None,
    Some(("sim-tb-logs", "icons/square-terminal.svg", "Device logs (logcat)", SimCommand::OpenLogs)),
    None,
    Some(("sim-tb-rotate", "icons/rotate-cw.svg", "Rotate  ⌘→", SimCommand::RotateCw)),
    None,
    Some(("sim-tb-power", "icons/power.svg", "Shut down emulator", SimCommand::Shutdown)),
    Some(("sim-tb-detach", "icons/log-out.svg", "Detach", SimCommand::Detach)),
];

/// `m:ss` for a recording's running time.
pub(crate) fn elapsed_label(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl SimulatorPanel {
    fn pill(&self) -> gpui::Div {
        let (theme, density) = (self.theme, self.density);
        div()
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
            .bg(theme.bg_panel)
    }

    pub(super) fn render_toolbar(&self, state: &PanelState, cx: &mut Context<Self>) -> AnyElement {
        if self.confirm_shutdown {
            return self.render_shutdown_confirm(cx);
        }
        let (theme, density) = (self.theme, self.density);
        let live = matches!(state, PanelState::Streaming);
        let attached = self.device(cx).is_some();
        let recording = self.recording_since(cx);
        let device = self.device(cx);
        let android = device.as_ref().is_some_and(|d| d.platform() == oximux_simulator::Platform::Android);
        // A phone is never shut down from here.
        let phone = device.as_ref().is_some_and(crate::shell::simulator::SimulatorHub::is_phone);
        let items: &[Option<(&str, &str, &str, SimCommand)>] = if android { &ANDROID_ITEMS } else { &ITEMS };
        let mut pill = self.pill();
        for &item in items {
            let Some((id, icon, tip, command)) = item else {
                pill = pill.child(div().w(px(1.0)).h(px(density.h_row * 0.6)).bg(theme.border_inactive));
                continue;
            };
            let enabled = match command {
                SimCommand::Detach => attached,
                // Recording can always be stopped; captures work on any
                // attached, streaming device.
                SimCommand::ToggleRecord => live || recording.is_some(),
                SimCommand::Shutdown => live && !phone,
                _ => live,
            };
            let mut button = Button::new(id).ghost().small().tooltip(tip).disabled(!enabled);
            let mut glyph = Icon::default().path(icon);
            if command == SimCommand::ToggleRecord
                && let Some(since) = recording
            {
                glyph = glyph.text_color(theme.status_error);
                button = button
                    .label(SharedString::from(elapsed_label(since.elapsed().as_secs())))
                    .tooltip("Stop recording  ⌘R");
            }
            let sink = self.command_sink.clone();
            pill = pill.child(button.icon(glyph).on_click(move |_, window: &mut Window, cx| match &sink {
                Some(sink) => sink(RootRequest::Run(command), window, cx),
                None => window.dispatch_action(command.action(), cx),
            }));
        }
        pill.into_any_element()
    }

    fn render_shutdown_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let ty = &self.typography;
        self.pill()
            .child(
                div()
                    .px(px(self.density.gap_inline))
                    .text_size(px(ty.t_body_sm))
                    .text_color(self.theme.fg_base)
                    .child(format!("Shut down {}?", self.device_name(cx))),
            )
            .child(
                Button::new("sim-shutdown-cancel")
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _window, cx| this.answer_shutdown(false, cx))),
            )
            .child(
                Button::new("sim-shutdown-ok")
                    .danger()
                    .small()
                    .label("Shut Down")
                    .on_click(cx.listener(|this, _, _window, cx| this.answer_shutdown(true, cx))),
            )
            .into_any_element()
    }

    /// When the attached device's recording started, if one is running.
    pub(super) fn recording_since(&self, cx: &gpui::App) -> Option<Instant> {
        let (hub, udid) = (self.hub.as_ref()?, self.device(cx)?);
        hub.read(cx).recording_since(&udid)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn elapsed_reads_minutes_and_seconds() {
        assert_eq!(super::elapsed_label(7), "0:07");
        assert_eq!(super::elapsed_label(605), "10:05");
    }
}
