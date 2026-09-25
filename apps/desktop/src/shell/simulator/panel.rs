//! [`SimulatorPanel`]: the right-sidebar iOS Simulator tab.
//!
//! One entity per window, owned by `WorkspaceRoot` and handed to every
//! per-project `RightSidebar` (the ports-panel pattern), so whichever sidebar
//! is on screen renders the same panel. It shows the device attached to the
//! **active tab's worktree**; `WorkspaceRoot` pushes worktree changes
//! ([`SimulatorPanel::set_worktree`]) and visibility
//! ([`SimulatorPanel::set_visible`]) — a hidden entity never renders, so it
//! cannot infer either itself.
//!
//! All device state lives in the app-wide [`SimulatorHub`]; the panel keeps
//! only per-view bookkeeping (a pending attach, its last error) and derives
//! its body through [`super::state::derive`].

use std::path::PathBuf;
use std::time::Duration;

use gpui::{Context, Entity, Subscription, Task};
use oximux_settings::{Density, Theme, Typography};
use oximux_simulator::DeviceId;
use oximux_simulator::registry::Phase;

use super::hub::{HubEvent, SimulatorHub, hub};
use super::state::{self, Inputs, PanelState};

pub(crate) use stream_row::settings;

mod bezel;
mod body;
mod header;
mod toolbar;
mod stream_row;
#[cfg(test)]
mod tests;

/// How often the setup checklist re-checks while it is on screen.
const SETUP_RECHECK: Duration = Duration::from_secs(3);

pub struct SimulatorPanel {
    hub: Option<Entity<SimulatorHub>>,
    worktree: Option<PathBuf>,
    visible: bool,
    /// An attach this panel asked for is in flight.
    attaching: bool,
    /// The sidebar is maximized for the simulator (the ⤢ button's state).
    maximized: bool,
    attach_error: Option<String>,
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    _hub_events: Option<Subscription>,
    /// Re-checks availability while the setup checklist is visible.
    _setup_poll: Option<Task<()>>,
    /// Tests draw every body without a hub behind them.
    #[cfg(test)]
    state_override: Option<PanelState>,
}

impl SimulatorPanel {
    pub fn new(theme: Theme, density: Density, typography: Typography, cx: &mut Context<Self>) -> Self {
        let hub = hub(cx);
        let subscription = hub.as_ref().map(|hub| cx.subscribe(hub, Self::on_hub_event));
        // The window is closing: stop counting this panel as a viewer, or its
        // helper keeps streaming for a window that no longer exists (macOS
        // keeps the app alive after the last window closes).
        cx.on_release(|panel: &mut Self, cx| {
            if let (Some(hub), Some(worktree)) = (panel.hub.clone(), panel.worktree.clone())
                && panel.visible
            {
                hub.update(cx, |hub, cx| hub.set_visible(&worktree, false, cx));
            }
        })
        .detach();
        Self {
            hub,
            worktree: None,
            visible: false,
            attaching: false,
            maximized: false,
            attach_error: None,
            theme,
            density,
            typography,
            _hub_events: subscription,
            _setup_poll: None,
            #[cfg(test)]
            state_override: None,
        }
    }

    fn on_hub_event(&mut self, _hub: Entity<SimulatorHub>, event: &HubEvent, cx: &mut Context<Self>) {
        match event {
            HubEvent::Changed(udid) => {
                // Looked up here, not per event: frames arrive 30-60 times a second.
                if self.device(cx).as_ref() == Some(udid) {
                    // The attach landed (or moved on): the phase speaks now.
                    self.attaching = false;
                    self.attach_error = None;
                }
                cx.notify();
            }
            HubEvent::AttachFailed(path, why) => {
                if self.worktree.as_ref() == Some(path) {
                    self.attaching = false;
                    self.attach_error = Some(why.clone());
                    cx.notify();
                }
            }
            HubEvent::Availability | HubEvent::Devices => cx.notify(),
            // Frames repaint the screen view (P6), not the whole panel.
            HubEvent::Frame(_) | HubEvent::Session(..) => {}
        }
    }

    /// The active tab's worktree changed (`None`: no project open).
    pub fn set_worktree(&mut self, worktree: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.worktree == worktree {
            return;
        }
        let visible = self.visible;
        if let Some(hub) = self.hub.clone() {
            hub.update(cx, |hub, cx| {
                if let Some(old) = &self.worktree {
                    hub.set_visible(old, false, cx);
                }
                if let Some(new) = &worktree {
                    hub.set_visible(new, visible, cx);
                }
            });
        }
        self.worktree = worktree;
        self.attaching = false;
        self.attach_error = None;
        cx.notify();
    }

    /// Whether the panel is on screen (sidebar open and this tab selected).
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        let Some(hub) = self.hub.clone() else { return };
        hub.update(cx, |hub, cx| {
            if let Some(worktree) = &self.worktree {
                hub.set_visible(worktree, visible, cx);
            }
            if visible {
                // `mark_used` runs the first availability check itself.
                if !hub.mark_used(cx) {
                    hub.refresh_availability(cx);
                }
                hub.refresh_devices(cx);
            }
        });
        self._setup_poll = visible.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(SETUP_RECHECK).await;
                    let still = this.update(cx, |panel, cx| {
                        if matches!(panel.state(cx), PanelState::Setup(_) | PanelState::Checking)
                            && let Some(hub) = panel.hub.clone()
                        {
                            hub.update(cx, |hub, cx| hub.refresh_availability(cx));
                        }
                    });
                    if still.is_err() {
                        return;
                    }
                }
            })
        });
        cx.notify();
    }

    pub fn set_maximized(&mut self, maximized: bool, cx: &mut Context<Self>) {
        if self.maximized != maximized {
            self.maximized = maximized;
            cx.notify();
        }
    }

    pub(crate) fn is_maximized(&self) -> bool {
        self.maximized
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The device attached to the current worktree.
    pub(crate) fn device(&self, cx: &gpui::App) -> Option<DeviceId> {
        let (hub, worktree) = (self.hub.as_ref()?, self.worktree.as_ref()?);
        hub.read(cx).device_for(worktree)
    }

    pub(crate) fn state(&self, cx: &gpui::App) -> PanelState {
        #[cfg(test)]
        if let Some(state) = &self.state_override {
            return state.clone();
        }
        let Some(hub) = self.hub.as_ref() else {
            return PanelState::Checking;
        };
        let hub = hub.read(cx);
        let device = self.worktree.as_ref().and_then(|w| hub.device_for(w));
        let phase = device.as_ref().map(|d| hub.phase(d)).unwrap_or(Phase::Idle);
        state::derive(Inputs {
            availability: hub.availability(),
            attached: device.is_some(),
            phase: &phase,
            attaching: self.attaching,
            attach_error: self.attach_error.as_deref(),
        })
    }

    /// Attach the current worktree to `device`, or to the automatic pick.
    pub(crate) fn attach(&mut self, device: Option<DeviceId>, cx: &mut Context<Self>) {
        let (Some(hub), Some(worktree)) = (self.hub.clone(), self.worktree.clone()) else { return };
        // "Attach simulator" on a restored attachment reconnects *that*
        // device; only a worktree with none gets the automatic pick.
        let device = device.or_else(|| self.device(cx));
        self.attaching = true;
        self.attach_error = None;
        let preferred = stream_row::settings(cx).default_device.map(DeviceId);
        hub.update(cx, |hub, cx| hub.attach(&worktree, device, preferred, cx));
        cx.notify();
    }

    pub(crate) fn detach(&mut self, cx: &mut Context<Self>) {
        let (Some(hub), Some(worktree)) = (self.hub.clone(), self.worktree.clone()) else { return };
        hub.update(cx, |hub, cx| hub.detach(&worktree, cx));
        self.attaching = false;
        self.attach_error = None;
        cx.notify();
    }

    pub(crate) fn reconnect(&mut self, cx: &mut Context<Self>) {
        let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) else { return };
        hub.update(cx, |hub, cx| hub.reconnect(&udid, cx));
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(hub) = self.hub.clone() {
            hub.update(cx, |hub, cx| {
                hub.refresh_availability(cx);
                hub.refresh_devices(cx);
            });
        }
    }
}

impl gpui::Render for SimulatorPanel {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _, div};
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let state = self.state(cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(self.theme.bg_panel)
            .child(self.render_header(&state, cx))
            .child(self.render_body(&state, cx))
    }
}
