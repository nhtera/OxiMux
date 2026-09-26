//! [`SimulatorPanel`]: the right-sidebar iOS Simulator tab.
//!
//! One entity per window, owned by `WorkspaceRoot` and handed to every
//! per-project `RightSidebar` (the ports-panel pattern), so whichever sidebar
//! is on screen renders the same panel. It shows the device attached to the
//! **active tab's worktree**; `WorkspaceRoot` pushes worktree changes
//! ([`SimulatorPanel::set_worktree`]) and visibility
//! ([`SimulatorPanel::set_visible`]) — a hidden entity never renders, so it
//! cannot infer either itself. The panel adds its window's own visibility
//! (minimized, covered, on another Space): the hub hears "shown" only while
//! both hold, and "hidden" only after [`HIDE_DEBOUNCE`], so flipping tabs
//! does not pause and resume the helper.
//!
//! All device state lives in the app-wide [`SimulatorHub`]; the panel keeps
//! only per-view bookkeeping (a pending attach, its last error) and derives
//! its body through [`super::state::derive`].

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task, Window};
use oximux_settings::{Density, Theme, Typography};
use oximux_simulator::DeviceId;
use oximux_simulator::registry::Phase;

use oximux_simulator::session::SessionEvent;

use super::annotate::AnnotateView;
use super::hub::{HubEvent, NoticeKind, SimulatorHub, hub};
use super::screen::ScreenView;
use super::state::{self, Inputs, PanelState};

pub(crate) use stream_row::settings;
pub(crate) use commands::{Outcome, SimCommand};

mod annotating;
mod bezel;
mod body;
mod commands;
mod consent;
mod header;
mod toolbar;
mod stream_row;
#[cfg(test)]
mod tests;

/// How often the setup checklist re-checks while it is on screen.
const SETUP_RECHECK: Duration = Duration::from_secs(3);
/// A hide must last this long before the helper is paused.
const HIDE_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct SimulatorPanel {
    hub: Option<Entity<SimulatorHub>>,
    worktree: Option<PathBuf>,
    /// Sidebar open on this tab (pushed by the root).
    visible: bool,
    /// The window is on screen (not minimized, covered or on another Space).
    window_visible: bool,
    /// What the hub was last told for `worktree`.
    shown: bool,
    _hide: Option<Task<()>>,
    _window_visibility: Option<Subscription>,
    /// The live screen, made on first stream (it needs a window).
    screen: Option<Entity<ScreenView>>,
    /// The phone's measured space (see `bezel::phone`).
    area: Rc<Cell<Option<(f32, f32)>>>,
    /// The toolbar is asking "Shut down …?".
    confirm_shutdown: bool,
    /// Annotate mode: the frozen screenshot being marked up.
    annotate: Option<Entity<AnnotateView>>,
    /// Re-renders once a second while a recording's timer shows.
    _record_tick: Option<Task<()>>,
    /// Runs a toolbar command through the window root (see `root_glue`).
    command_sink: Option<CommandSink>,
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
                && panel.shown
            {
                hub.update(cx, |hub, cx| hub.set_visible(&worktree, false, cx));
            }
        })
        .detach();
        Self {
            hub,
            worktree: None,
            visible: false,
            window_visible: true,
            shown: false,
            _hide: None,
            _window_visibility: None,
            screen: None,
            area: Rc::default(),
            confirm_shutdown: false,
            annotate: None,
            _record_tick: None,
            command_sink: None,
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
            HubEvent::Notice(udid, kind, text) => {
                if self.device(cx).as_ref() == Some(udid) {
                    cx.emit(PanelEvent::Notice(*kind, text.clone()));
                }
            }
            HubEvent::AttachFailed(path, why) => {
                if self.worktree.as_ref() == Some(path) {
                    self.attaching = false;
                    self.attach_error = Some(why.clone());
                    cx.notify();
                }
            }
            HubEvent::Availability | HubEvent::Devices | HubEvent::Consent => cx.notify(),
            HubEvent::AgentActivity(udid) => {
                if self.device(cx).as_ref() == Some(udid) {
                    cx.notify();
                }
            }
            // The phone outline follows the stream's size and orientation.
            HubEvent::Session(udid, SessionEvent::Size { .. } | SessionEvent::Orientation(_)) => {
                if self.device(cx).as_ref() == Some(udid) {
                    cx.notify();
                }
            }
            HubEvent::DeviceBooted(udids) => cx.emit(PanelEvent::DeviceBooted(udids.clone())),
            // Frames repaint the screen view, not the whole panel.
            HubEvent::Frame(_) | HubEvent::Session(..) => {}
        }
    }

    /// The active tab's worktree changed (`None`: no project open).
    pub fn set_worktree(&mut self, worktree: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.worktree == worktree {
            return;
        }
        let shown = self.visible && self.window_visible;
        if let Some(hub) = self.hub.clone() {
            // Show the new one first: two worktrees on one device then never
            // see a pause-resume blip.
            hub.update(cx, |hub, cx| {
                if let Some(new) = worktree.as_ref().filter(|_| shown) {
                    hub.set_visible(new, true, cx);
                }
                if let Some(old) = self.worktree.as_ref().filter(|_| self.shown) {
                    hub.set_visible(old, false, cx);
                }
            });
        }
        self.shown = shown;
        self._hide = None;
        self.worktree = worktree;
        // Both were about the old worktree's device.
        self.confirm_shutdown = false;
        self.annotate = None;
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
        self.push_shown(cx);
        let Some(hub) = self.hub.clone() else { return };
        hub.update(cx, |hub, cx| {
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

    /// Tell the hub whether the panel can be seen: at once when it can, after
    /// [`HIDE_DEBOUNCE`] when it cannot.
    fn push_shown(&mut self, cx: &mut Context<Self>) {
        let shown = self.visible && self.window_visible;
        if !shown && let Some(screen) = self.screen.clone() {
            // Decoding stops at once; only the helper's pause is debounced.
            screen.update(cx, |screen, cx| screen.hide(cx));
        }
        if shown {
            self._hide = None;
            self.set_shown(true, cx);
        } else if self.shown && self._hide.is_none() {
            self._hide = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HIDE_DEBOUNCE).await;
                let _ = this.update(cx, |panel, cx| {
                    panel._hide = None;
                    if !(panel.visible && panel.window_visible) {
                        panel.set_shown(false, cx);
                    }
                });
            }));
        }
    }

    fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if let (Some(hub), Some(worktree)) = (self.hub.clone(), self.worktree.clone()) {
            hub.update(cx, |hub, cx| hub.set_visible(&worktree, shown, cx));
        }
    }

    /// Follow the window's own visibility; registered on first render, the
    /// first point with a window at hand.
    fn observe_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._window_visibility.is_some() {
            return;
        }
        self.window_visible = window.visibility().is_visible();
        self._window_visibility = Some(cx.observe_window_visibility(window, |panel, visibility, _window, cx| {
            panel.window_visible = visibility.is_visible();
            panel.push_shown(cx);
            // Back on screen: render, so the screen view takes a fresh frame.
            cx.notify();
        }));
    }

    /// The live screen, bound to `device` for this render.
    fn live_screen(&mut self, binding: super::screen::Binding, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<ScreenView>> {
        let hub = self.hub.clone()?;
        let (theme, typography) = (self.theme, self.typography.clone());
        let screen = self
            .screen
            .get_or_insert_with(|| cx.new(|cx| ScreenView::new(hub, theme, typography.clone(), window, cx)))
            .clone();
        screen.update(cx, |screen, cx| screen.bind(binding, theme, &typography, window, cx));
        Some(screen)
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
            android_ready: hub.android_sdk().is_some(),
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
        hub.update(cx, |hub, cx| {
            // The user's own Reconnect lifts the power button's "stop".
            hub.clear_stopped_by_user(&udid);
            hub.reconnect(&udid, cx);
        });
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

/// What the panel asks of the window root from a click or a finished task:
/// with nothing focused, a dispatched action would never reach the root.
pub(crate) enum RootRequest {
    /// Run a command the way its action would.
    Run(SimCommand),
    /// Stage an annotated screenshot in the active agent.
    SendToAgent(crate::actions::SendPickToActiveChat),
}

/// Carries [`RootRequest`]s to the root. Never call it from inside an update
/// of the panel: the root updates the panel in turn.
pub(crate) type CommandSink = Rc<dyn Fn(RootRequest, &mut Window, &mut gpui::App)>;

impl SimulatorPanel {
    pub(crate) fn set_command_sink(&mut self, sink: CommandSink) {
        self.command_sink = Some(sink);
    }
}

/// What the window hears from the panel.
#[derive(Clone, Debug)]
pub enum PanelEvent {
    /// Show this as a toast.
    Notice(NoticeKind, String),
    /// Devices nobody attached booted (the window may attach one where an
    /// agent is working; see `auto_open`).
    DeviceBooted(Vec<DeviceId>),
}

impl EventEmitter<PanelEvent> for SimulatorPanel {}

impl SimulatorPanel {
    /// The screen's corner radius for the current layout (0 before the
    /// first measurement).
    pub(super) fn screen_radius(&self) -> f32 {
        self.area.get().and_then(|a| bezel::fit(a, &bezel::Device::PLACEHOLDER)).map_or(0.0, |l| l.screen_radius)
    }

    /// Keep a recording's timer ticking while it shows.
    fn tick_recording(&mut self, cx: &mut Context<Self>) {
        let on = self.recording_since(cx).is_some();
        if on == self._record_tick.is_some() {
            return;
        }
        self._record_tick = on.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            })
        });
    }
}

impl gpui::Render for SimulatorPanel {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _, div};
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        self.observe_window(window, cx);
        self.tick_recording(cx);
        let state = self.state(cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(self.theme.bg_panel)
            .child(self.render_header(&state, cx))
            .child(self.render_body(&state, window, cx))
    }
}
