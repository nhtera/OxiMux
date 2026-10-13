//! Recording a flow from the panel: the Device menu's Record flow, the red
//! "● REC n steps" chip while it runs (a click stops it), and the user's own
//! input as steps (see `screen::recording`; the toolbar's buttons here).
//!
//! It is the user's recording: no `oximux sim` verb can start, read or stop
//! it. Stopping saves it in the app's data (private) and copies the steps,
//! so they can be pasted into a file in the worktree or a chat.

use gpui::{AnyElement, ClipboardItem, Context, IntoElement, SharedString, Styled as _};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_remote_proto::simulator::{SimButtonWire, SimOrientationWire};
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::{DeviceId, Orientation};

use super::commands::SimCommand;
use super::{PanelEvent, SimulatorPanel};
use crate::shell::simulator::agent_ops::{flow_platform, screen_points};
use crate::shell::simulator::hub::{HubEvent, NoticeKind, PanelStep, RecSource, save_flow};

impl SimulatorPanel {
    fn rec_key(&self, cx: &gpui::App) -> Option<(WorktreeKey, DeviceId, RecSource)> {
        Some((WorktreeKey::from_path(self.worktree.as_ref()?), self.device(cx)?, RecSource::Panel))
    }

    /// The steps recorded so far, while the panel records the shown device.
    pub(super) fn flow_steps(&self, cx: &gpui::App) -> Option<u32> {
        let (hub, key) = (self.hub.as_ref()?, self.rec_key(cx)?);
        hub.read(cx).flows().get(&key).map(|r| r.count())
    }

    pub(super) fn start_flow(&mut self, cx: &mut Context<Self>) {
        let (Some(hub), Some(key)) = (self.hub.clone(), self.rec_key(cx)) else { return };
        hub.update(cx, |hub, cx| {
            let screen = screen_points(hub, &key.1);
            let platform = flow_platform(&key.1);
            if hub.flows_mut().start(key, platform, screen) {
                cx.emit(HubEvent::Flow);
            }
        });
    }

    /// Stop: save it privately, copy the steps, and say where it went.
    pub(super) fn stop_flow(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.rec_key(cx) else { return };
        // A tap still reading its element is done within its read budget:
        // waited out, not dropped (before the screen's size is known it has
        // no point to fall back on).
        let Some(wait) = self.screen.as_ref().and_then(|s| s.read(cx).recording_settles_in()) else {
            return self.finish_stop(key, cx);
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| this.finish_stop(key, cx));
        })
        .detach();
    }

    fn finish_stop(&mut self, key: (WorktreeKey, DeviceId, RecSource), cx: &mut Context<Self>) {
        // Switched away meanwhile: the switch saved it.
        let Some(hub) = self.hub.clone().filter(|_| self.rec_key(cx).as_ref() == Some(&key)) else { return };
        // What the screen still holds in line (a tap reading its element,
        // typing behind it) goes in before the stop.
        let pending = self.screen.clone().map(|s| s.update(cx, |s, _| s.drain_recording())).unwrap_or_default();
        for step in pending {
            self.record_panel(step, cx);
        }
        let stopped = hub.update(cx, |hub, cx| {
            let stopped = hub.flows_mut().stop(&key);
            cx.emit(HubEvent::Flow);
            stopped
        });
        let Some(recording) = stopped else { return };
        let (text, count) = (recording.text(), recording.count());
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        let worktree = key.0;
        cx.spawn(async move |this, cx| {
            let saved = cx.background_executor().spawn(async move { save_flow(&worktree, &text) }).await;
            let _ = this.update(cx, |_, cx| {
                let notice = match saved {
                    Ok(path) => (
                        NoticeKind::Success,
                        format!("Flow saved ({count} step{}), steps copied: {}", if count == 1 { "" } else { "s" }, path.display()),
                    ),
                    Err(why) => (NoticeKind::Error, format!("Flow not saved ({why}); its steps are copied")),
                };
                cx.emit(PanelEvent::Notice(notice.0, notice.1));
            });
        })
        .detach();
    }

    /// One piece of the user's input, while recording.
    pub(super) fn record_panel(&mut self, step: PanelStep, cx: &mut Context<Self>) {
        let (Some(hub), Some(key)) = (self.hub.clone(), self.rec_key(cx)) else { return };
        hub.update(cx, |hub, cx| {
            let screen = screen_points(hub, &key.1);
            if let Some(recording) = hub.flows_mut().get_mut(&key) {
                recording.fill_screen(screen);
                recording.panel(step);
                cx.emit(HubEvent::Flow);
            }
        });
    }

    /// A toolbar command that ran, as a step (rotation: where it turned to,
    /// from `before`).
    pub(super) fn record_command(&mut self, command: SimCommand, before: Option<Orientation>, cx: &mut Context<Self>) {
        let step = match command {
            SimCommand::Home => PanelStep::Button(SimButtonWire::Home),
            SimCommand::Lock => PanelStep::Button(SimButtonWire::Lock),
            SimCommand::Back => PanelStep::Button(SimButtonWire::Back),
            SimCommand::Recents => PanelStep::Button(SimButtonWire::AppSwitcher),
            SimCommand::VolumeUp => PanelStep::Button(SimButtonWire::VolumeUp),
            SimCommand::VolumeDown => PanelStep::Button(SimButtonWire::VolumeDown),
            SimCommand::RotateCw | SimCommand::RotateCcw => {
                let Some(now) = before else { return };
                let next = if command == SimCommand::RotateCw { now.rotated_right() } else { now.rotated_left() };
                PanelStep::Rotate(match next {
                    Orientation::Portrait => SimOrientationWire::Portrait,
                    Orientation::LandscapeLeft => SimOrientationWire::LandscapeLeft,
                    Orientation::LandscapeRight => SimOrientationWire::LandscapeRight,
                    Orientation::PortraitUpsideDown => SimOrientationWire::UpsideDown,
                })
            }
            _ => return,
        };
        // In line with the screen's taps and typing (a tap may still be
        // reading its element).
        match self.screen.clone() {
            Some(screen) => screen.update(cx, |screen, cx| screen.record_step(step, cx)),
            None => self.record_panel(step, cx),
        }
    }

    /// The chip, while recording: a click stops it.
    pub(super) fn render_rec_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.flow_steps(cx)?;
        let theme = self.theme;
        let label = format!("REC {count} step{}", if count == 1 { "" } else { "s" });
        Some(
            Button::new("sim-flow-rec")
                .ghost()
                .small()
                .icon(Icon::default().path("icons/circle-dot.svg").text_color(theme.status_error))
                .label(SharedString::from(label))
                .tooltip("Stop recording the flow: it is saved and its steps copied")
                .on_click(cx.listener(|this, _, _, cx| this.stop_flow(cx)))
                .into_any_element(),
        )
    }
}
