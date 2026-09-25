//! [`ScreenView`]: the live device screen inside the panel's phone outline.
//!
//! **Frames.** On each [`HubEvent::Frame`] for its device the view takes the
//! session's newest frame, decodes it on the background executor
//! ([`decode`]) and swaps it in. At most one decode is in flight; frames that
//! land meanwhile collapse into one more decode of whatever is newest then
//! (latest wins). The previous image leaves the GPU atlas in the same update
//! that replaces it, so at most two frame images are ever alive. Nothing is
//! decoded while the panel is hidden.
//!
//! **Why not a cached view.** `AnyView::cached` would not isolate the frame
//! notifies (a notify dirties every ancestor view anyway), and it would miss
//! what the panel hands over in [`ScreenView::bind`] during its own render: a
//! notify raised mid-draw is not seen by that draw's cache check.
//!
//! Input lives in [`input`] (touch, scroll, pinch) and [`keys`] (keyboard
//! capture, Esc, paste).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement as _, Pixels, Render, RenderImage,
    Styled as _, StyledImage as _, Subscription, Task, Window, canvas, div, img, px,
};
use oximux_settings::{Theme, Typography};
use oximux_simulator::DeviceId;
use oximux_simulator::gesture::WheelDrag;
use oximux_simulator::session::StreamSession;

use super::hub::{HubEvent, SimulatorHub};
use super::panel::settings;

mod decode;
mod input;
mod keys;

pub use keys::register_screen_key_bindings;

/// Key context of the focused screen (keyboard capture).
pub const SIMULATOR_SCREEN_KEY_CONTEXT: &str = "SimulatorScreen";

/// What the panel hands the view each render.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Binding {
    pub device: Option<DeviceId>,
    /// The panel is on screen (tab selected, sidebar open, window visible).
    pub visible: bool,
    /// Corner radius of the screen inside the bezel, for the capture ring.
    pub radius: f32,
}

pub struct ScreenView {
    hub: Entity<SimulatorHub>,
    binding: Binding,
    image: Option<Arc<RenderImage>>,
    /// `(helper pid, frame seq)` of the last frame taken: a new session's
    /// sequence restarts at zero.
    taken: Option<(u32, u64)>,
    decoding: bool,
    /// A frame arrived while decoding: decode again when done.
    behind: bool,
    /// The screen's window bounds, recorded each prepaint (input mapping).
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Option<input::Drag>,
    wheel: WheelDrag,
    wheel_state: input::WheelState,
    _wheel_idle: Option<Task<()>>,
    focus: FocusHandle,
    /// The "typing goes to the simulator" hint shows until then.
    hint_until: Option<Instant>,
    _hint: Option<Task<()>>,
    /// When each recently painted frame first reached the screen (the FPS
    /// readout counts paints, not decodes: an inactive window draws at most
    /// 30 times a second whatever arrives).
    painted: Rc<RefCell<Painted>>,
    _fps_tick: Option<Task<()>>,
    theme: Theme,
    typography: Typography,
    _hub_events: Subscription,
    _activation: Subscription,
}

/// Paint-side FPS bookkeeping: the image last painted, and when each new one
/// was painted over the last second.
#[derive(Default)]
struct Painted {
    last: Option<gpui::ImageId>,
    at: VecDeque<Instant>,
}

impl Painted {
    fn record(&mut self, image: gpui::ImageId, now: Instant) {
        if self.last.replace(image) != Some(image) {
            self.at.push_back(now);
        }
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self.at.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.at.pop_front();
        }
    }
}

impl ScreenView {
    pub(crate) fn new(
        hub: Entity<SimulatorHub>,
        theme: Theme,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(&hub, window, Self::on_hub_event);
        // Switching apps mid-drag delivers no mouse-up: lift the finger.
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.release_input(cx);
            }
        });
        Self {
            hub,
            binding: Binding { device: None, visible: false, radius: 0.0 },
            image: None,
            taken: None,
            decoding: false,
            behind: false,
            bounds: Rc::default(),
            drag: None,
            wheel: WheelDrag::default(),
            wheel_state: input::WheelState::default(),
            _wheel_idle: None,
            focus: cx.focus_handle(),
            hint_until: None,
            _hint: None,
            painted: Rc::default(),
            _fps_tick: None,
            theme,
            typography,
            _hub_events: subscription,
            _activation: activation,
        }
    }

    /// Called from the panel's render; this view renders right after, so no
    /// notify is needed (and one raised mid-draw would be lost anyway).
    pub(crate) fn bind(&mut self, binding: Binding, theme: Theme, typography: &Typography, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = theme;
        self.typography = typography.clone();
        if binding == self.binding {
            return;
        }
        let device_changed = binding.device != self.binding.device;
        let shown = binding.visible && !self.binding.visible;
        if device_changed {
            self.release_input(cx);
            if let Some(old) = self.image.take() {
                cx.drop_image(old, Some(window));
            }
            self.taken = None;
            *self.painted.borrow_mut() = Painted::default();
        }
        self.binding = binding;
        if device_changed || shown {
            self.pump(window, cx);
        }
    }

    /// The panel went out of sight (tab, sidebar, window): stop decoding now
    /// rather than at its next render, which a hidden panel never has.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        if self.binding.visible {
            self.binding.visible = false;
            self.release_input(cx);
        }
    }

    fn session(&self, cx: &App) -> Option<StreamSession> {
        self.hub.read(cx).session(self.binding.device.as_ref()?)
    }

    fn on_hub_event(&mut self, _hub: &Entity<SimulatorHub>, event: &HubEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let HubEvent::Frame(udid) = event
            && self.binding.device.as_ref() == Some(udid)
        {
            self.pump(window, cx);
        }
    }

    /// Decode the newest frame, unless one is in flight (then once more when
    /// it lands) or nobody can see it.
    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.binding.visible {
            return;
        }
        if self.decoding {
            self.behind = true;
            return;
        }
        let Some(session) = self.session(cx) else { return };
        let pid = session.pid();
        let seen = self.taken.filter(|(p, _)| *p == pid).map_or(0, |(_, seq)| seq);
        let Some((seq, frame)) = session.latest_frame(seen) else { return };
        self.taken = Some((pid, seq));
        self.decoding = true;
        let decoded = cx.background_executor().spawn(async move { decode::decode(&frame) });
        cx.spawn_in(window, async move |this, cx| {
            let decoded = decoded.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.decoding = false;
                match decoded {
                    // A frame for a device switched away from meanwhile is
                    // dropped: `bind` already cleared `taken`.
                    Ok(image) if view.taken.is_some_and(|(p, _)| p == pid) => view.show(image, window, cx),
                    Ok(_) => {}
                    Err(e) => tracing::debug!("simulator frame skipped: {e}"),
                }
                if std::mem::take(&mut view.behind) {
                    view.pump(window, cx);
                }
            });
        })
        .detach();
    }

    fn show(&mut self, image: Arc<RenderImage>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.image.replace(image) {
            cx.drop_image(old, Some(window));
        }
        cx.notify();
    }

    /// While the readout shows, refresh it once a second (a still screen
    /// sends no frames, so nothing else would bring it down to 0).
    fn tick_fps(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self._fps_tick.is_some() {
            return;
        }
        self._fps_tick = on.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let alive = this.update(cx, |view, cx| {
                        view.painted.borrow_mut().prune(Instant::now());
                        cx.notify();
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            })
        });
    }

    fn render_fps(&self) -> AnyElement {
        let ty = &self.typography;
        div()
            .absolute()
            .top(px(8.))
            .right(px(8.))
            .px(px(6.))
            .py(px(2.))
            .rounded(px(4.))
            .bg(self.theme.bg_overlay)
            .font_family(ty.family_mono.clone())
            .text_size(px(ty.t_body_sm))
            .text_color(self.theme.fg_base)
            .child(format!("{} fps", self.painted.borrow().at.len()))
            .into_any_element()
    }

    fn render_hint(&self) -> AnyElement {
        let ty = &self.typography;
        div()
            .absolute()
            .bottom(px(24.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .bg(self.theme.bg_overlay)
                    .text_size(px(ty.t_body_sm))
                    .text_color(self.theme.fg_base)
                    .child("Typing goes to the simulator · ⌃Esc to release"),
            )
            .into_any_element()
    }
}

impl Render for ScreenView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let captured = self.focus.is_focused(window);
        let hint = captured && self.hint_until.is_some_and(|t| Instant::now() < t);
        let show_fps = settings(cx).stream.show_fps && self.binding.visible;
        self.tick_fps(show_fps, cx);
        let bounds = self.bounds.clone();
        let weak = cx.weak_entity();
        let painted = self.painted.clone();
        let image_id = self.image.as_ref().map(|i| i.id);
        // Window-level drag listeners, so a drag keeps moving (and always
        // ends) when the pointer leaves the screen. Re-registered each paint;
        // they do nothing unless a drag is in progress.
        let meter = canvas(
            move |b, _, _| bounds.set(Some(b)),
            move |_, _, window, _| {
                if let Some(id) = image_id {
                    painted.borrow_mut().record(id, Instant::now());
                }
                let on_move = weak.clone();
                // Capture phase: nothing drawn later can swallow them.
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _window, cx| {
                    if phase == DispatchPhase::Capture && e.pressed_button == Some(MouseButton::Left) {
                        let _ = on_move.update(cx, |view, cx| view.drag_to(e.position, e.modifiers.alt, cx));
                    }
                });
                let on_up = weak;
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, cx| {
                    if phase == DispatchPhase::Capture && e.button == MouseButton::Left {
                        let _ = on_up.update(cx, |view, cx| view.drag_end(e.position, cx));
                    }
                });
            },
        )
        // Pinned: an absolute box without insets keeps its in-flow spot
        // (below the image), which would offset every input point.
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let mut screen = div()
            .id("sim-screen")
            .track_focus(&self.focus)
            .key_context(SIMULATOR_SCREEN_KEY_CONTEXT)
            .relative()
            .size_full()
            .bg(gpui::black())
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down_out(cx.listener(|view, _, window, cx| {
                if view.focus.is_focused(window) {
                    window.blur(cx);
                }
            }))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_paste));
        if let Some(image) = self.image.clone() {
            // Rounded itself: the parent's `overflow_hidden` clips to a
            // rectangle, not to the screen's rounded corners.
            screen = screen
                .child(img(image).size_full().object_fit(ObjectFit::Contain).rounded(px(self.binding.radius)));
        }
        screen = screen.child(meter);
        if captured {
            screen = screen.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(self.binding.radius))
                    .border_2()
                    .border_color(self.theme.focus_ring),
            );
        }
        if hint {
            screen = screen.child(self.render_hint());
        }
        if show_fps {
            screen = screen.child(self.render_fps());
        }
        screen
    }
}
