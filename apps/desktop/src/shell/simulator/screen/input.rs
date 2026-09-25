//! Pointer input on the screen: click/drag → a live touch, Option+drag → a
//! two-finger pinch mirrored about the screen centre, wheel/trackpad → a
//! one-finger drag ([`WheelDrag`]).
//!
//! Points go window → the frame's letterboxed rect → display-normalized →
//! portrait-normalized (what the helper takes in every orientation). A touch
//! begun on the home-indicator band carries its edge for the whole gesture.

use std::time::{Duration, Instant};

use gpui::{Context, MouseDownEvent, Pixels, Point, ScrollWheelEvent, TouchPhase as WheelPhase, Window, px};
use oximux_simulator::geometry::{self, Rect, Size};
use oximux_simulator::gesture::{TouchStep, mirror};
use oximux_simulator::protocol::{Command, TouchPhase};

use super::ScreenView;

/// Moves closer together than this are coalesced (≈ one 60 Hz frame).
const MOVE_INTERVAL: Duration = Duration::from_millis(16);
/// The wheel counts as idle (the finger lifts) after this long without a
/// delta. Trackpad gestures also end on their own `Ended` phase.
const WHEEL_IDLE: Duration = Duration::from_millis(120);
/// A delta after a longer gap than this starts a new wheel gesture (so a
/// mouse wheel after a trackpad fling is not taken for its momentum).
const WHEEL_GAP: Duration = Duration::from_millis(150);
/// Pixels per wheel "line" for mice that report lines (a notch).
const LINE_HEIGHT: f32 = 40.0;
/// Where an Option-drag pinch mirrors its second finger.
const PINCH_CENTER: (f64, f64) = (0.5, 0.5);

/// A mouse drag in progress.
pub(super) struct Drag {
    /// Edge code from the begin point (0: none).
    edge: u32,
    pinch: bool,
    sent_at: Instant,
    /// The helper that saw the begin: a drag outliving its session (a
    /// restart mid-gesture) is dropped, never continued on the new one.
    pid: u32,
    /// Last display point sent, where a forced release lifts the finger.
    last: (f64, f64),
}

/// Trackpad bookkeeping: after the fingers lift, macOS keeps sending
/// momentum deltas (phase-less, like a mouse wheel's). iOS applies its own
/// momentum to the lifted finger, so ours is dropped.
#[derive(Default)]
pub(super) struct WheelState {
    in_momentum: bool,
    last: Option<Instant>,
}

impl ScreenView {
    /// The frame's painted rect in window coordinates, if a frame is up.
    fn frame_rect(&self) -> Option<Rect> {
        let bounds = self.bounds.get()?;
        let size = self.image.as_ref()?.size(0);
        let fit = geometry::letterbox(
            Size::new(f64::from(f32::from(bounds.size.width)), f64::from(f32::from(bounds.size.height))),
            Size::new(f64::from(size.width.0), f64::from(size.height.0)),
        );
        Some(Rect::new(f64::from(f32::from(bounds.origin.x)) + fit.x, f64::from(f32::from(bounds.origin.y)) + fit.y, fit.w, fit.h))
    }

    /// `pos` as a display-normalized point: `None` outside the frame unless
    /// `clamp`, which pins it to the nearest edge (a drag that left it).
    fn display_point(&self, pos: Point<Pixels>, clamp: bool) -> Option<(f64, f64)> {
        let rect = self.frame_rect()?;
        let (x, y) = (f64::from(f32::from(pos.x)), f64::from(f32::from(pos.y)));
        if clamp {
            if rect.w <= 0.0 || rect.h <= 0.0 {
                return None;
            }
            return Some((((x - rect.x) / rect.w).clamp(0.0, 1.0), ((y - rect.y) / rect.h).clamp(0.0, 1.0)));
        }
        geometry::to_normalized((x, y), rect)
    }

    fn portrait(&self, p: (f64, f64), cx: &Context<Self>) -> (f64, f64) {
        let orientation = self.session(cx).map(|s| s.orientation()).unwrap_or(oximux_simulator::Orientation::Portrait);
        geometry::display_to_portrait(orientation, p)
    }

    pub(super) fn send(&self, command: &Command, cx: &Context<Self>) {
        if let Some(session) = self.session(cx) {
            let _ = session.send(command);
        }
    }

    fn send_touch(&self, phase: TouchPhase, p: (f64, f64), edge: u32, pinch: bool, cx: &Context<Self>) {
        let command = if pinch {
            let (x1, y1) = self.portrait(p, cx);
            let (x2, y2) = self.portrait(mirror(p, PINCH_CENTER), cx);
            Command::Multitouch { phase, x1, y1, x2, y2 }
        } else {
            let (x, y) = self.portrait(p, cx);
            Command::Touch { phase, x, y, edge }
        };
        self.send(&command, cx);
    }

    pub(super) fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.capture_keyboard(window, cx);
        let Some(p) = self.display_point(event.position, false) else { return };
        let Some(pid) = self.session(cx).map(|s| s.pid()) else { return };
        self.release_input(cx);
        let pinch = event.modifiers.alt;
        let edge = if pinch { 0 } else { self.edge_at(p, cx) };
        self.send_touch(TouchPhase::Begin, p, edge, pinch, cx);
        self.drag = Some(Drag { edge, pinch, sent_at: Instant::now(), pid, last: p });
    }

    /// The drag, if its session is still the live one (else it is dropped).
    fn live_drag(&mut self, cx: &Context<Self>) -> Option<&mut Drag> {
        let pid = self.session(cx).map(|s| s.pid());
        if self.drag.as_ref().is_some_and(|d| Some(d.pid) != pid) {
            self.drag = None;
        }
        self.drag.as_mut()
    }

    fn edge_at(&self, p: (f64, f64), cx: &Context<Self>) -> u32 {
        let orientation = self.session(cx).map(|s| s.orientation()).unwrap_or(oximux_simulator::Orientation::Portrait);
        geometry::edge_for(orientation, p).unwrap_or(0)
    }

    /// A window-level move while the left button is down.
    pub(super) fn drag_to(&mut self, pos: Point<Pixels>, _alt: bool, cx: &mut Context<Self>) {
        let Some(drag) = self.live_drag(cx) else { return };
        if drag.sent_at.elapsed() < MOVE_INTERVAL {
            return;
        }
        let (edge, pinch) = (drag.edge, drag.pinch);
        let Some(p) = self.display_point(pos, true) else { return };
        self.send_touch(TouchPhase::Move, p, edge, pinch, cx);
        if let Some(drag) = self.drag.as_mut() {
            drag.sent_at = Instant::now();
            drag.last = p;
        }
    }

    /// A window-level left-button release: ends the touch wherever it is.
    pub(super) fn drag_end(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.live_drag(cx).is_none() {
            return;
        }
        let Some(drag) = self.drag.take() else { return };
        let p = self.display_point(pos, true).unwrap_or(drag.last);
        self.send_touch(TouchPhase::End, p, drag.edge, drag.pinch, cx);
    }

    pub(super) fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let now = Instant::now();
        let fresh = self.wheel_state.last.is_none_or(|t| now.duration_since(t) > WHEEL_GAP);
        self.wheel_state.last = Some(now);
        match event.touch_phase {
            WheelPhase::Started => self.wheel_state.in_momentum = false,
            WheelPhase::Moved if fresh => self.wheel_state.in_momentum = false,
            WheelPhase::Moved if self.wheel_state.in_momentum => return,
            WheelPhase::Moved => {}
            // The fingers lifted: lift ours; what follows is momentum.
            _ => {
                self.wheel_state.in_momentum = true;
                self.finish_wheel(cx);
                return;
            }
        }
        if self.drag.is_some() {
            return;
        }
        let (Some(rect), Some(cursor)) = (self.frame_rect(), self.display_point(event.position, true)) else { return };
        let delta = event.delta.pixel_delta(px(LINE_HEIGHT));
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        // GPUI's delta is the content's motion; the finger moves with it.
        let d = (f64::from(f32::from(delta.x)) / rect.w, f64::from(f32::from(delta.y)) / rect.h);
        if d == (0.0, 0.0) {
            return;
        }
        let steps = self.wheel.scroll(cursor, d);
        self.send_steps(&steps, cx);
        self._wheel_idle = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(WHEEL_IDLE).await;
            let _ = this.update(cx, |view, cx| view.finish_wheel(cx));
        }));
    }

    fn send_steps(&self, steps: &[TouchStep], cx: &Context<Self>) {
        for step in steps {
            self.send_touch(step.phase, (step.x, step.y), 0, false, cx);
        }
    }

    fn finish_wheel(&mut self, cx: &mut Context<Self>) {
        self._wheel_idle = None;
        if let Some(step) = self.wheel.finish() {
            self.send_steps(&[step], cx);
        }
    }

    /// Lift every finger where it last was (the device is being switched
    /// away from, the app lost focus, or the panel was hidden).
    pub(super) fn release_input(&mut self, cx: &mut Context<Self>) {
        self.finish_wheel(cx);
        if self.live_drag(cx).is_some()
            && let Some(drag) = self.drag.take()
        {
            self.send_touch(TouchPhase::End, drag.last, drag.edge, drag.pinch, cx);
        }
    }
}
