//! Touch gesture sequencing: tap, swipe, long-press, each expanded into the
//! `begin`/`move`/`end` [`TouchStep`]s the helper's `touch{phase,x,y,edge?}`
//! command takes (`phase-03-simulator-core-crate.md`'s protocol table).
//!
//! All coordinates here are **portrait-normalized** (`0..1` in the
//! framebuffer, not the display) — see [`crate::geometry`] for converting a
//! display-space point or an AX frame into this space first, and for
//! [`crate::geometry::edge_for`], which callers building a bottom-edge
//! swipe-to-home gesture must merge into the `begin` step (and keep for
//! every step after it: upstream's `moveDuoTouch`,
//! `duo-home-gesture.ts:14-16`, never recomputes the edge mid-gesture).
//!
//! Step timing at 16 ms ≈ one frame at 60 Hz, the cadence the P1 spike's
//! touch driver used (`spike-report.md` §2's "continuous 60 Hz touch drag").

use crate::protocol::TouchPhase;

/// One step of a touch sequence, in portrait-normalized coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchStep {
    pub phase: TouchPhase,
    pub x: f64,
    pub y: f64,
    /// Time to wait after the *previous* step before sending this one. The
    /// first step's delay is always 0.
    pub delay_ms: u64,
}

/// Gap between consecutive `move`/`end` steps, matching the input's own key
/// pacing order of magnitude (`session.rs`'s writer thread paces keys at
/// 4 ms; touches use a coarser ~60 Hz step since a drag doesn't need
/// per-scanline granularity).
pub const STEP_MS: u64 = 16;

fn clamp(x: f64, y: f64) -> (f64, f64) {
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

/// A tap: touch down, held for one step, then released. iOS needs a
/// non-zero hold to tell a tap from a hover-adjacent glitch, so `end` carries
/// [`STEP_MS`]'s delay rather than 0.
pub fn tap(x: f64, y: f64) -> Vec<TouchStep> {
    let (x, y) = clamp(x, y);
    vec![
        TouchStep { phase: TouchPhase::Begin, x, y, delay_ms: 0 },
        TouchStep { phase: TouchPhase::End, x, y, delay_ms: STEP_MS },
    ]
}

/// A straight-line drag from `from` to `to` over `duration_ms`, stepped every
/// [`STEP_MS`]. Always emits at least one step between `begin` and `end`,
/// even when `duration_ms < STEP_MS` — the minimum granularity this module
/// supports is one step, so a very short "swipe" still moves before it ends.
pub fn swipe(from: (f64, f64), to: (f64, f64), duration_ms: u64) -> Vec<TouchStep> {
    let from = clamp(from.0, from.1);
    let to = clamp(to.0, to.1);
    let steps = (duration_ms / STEP_MS).max(1);
    let mut out = Vec::with_capacity(steps as usize + 1);
    out.push(TouchStep { phase: TouchPhase::Begin, x: from.0, y: from.1, delay_ms: 0 });
    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let (x, y) = clamp(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        out.push(TouchStep { phase: TouchPhase::Move, x, y, delay_ms: STEP_MS });
    }
    out.push(TouchStep { phase: TouchPhase::End, x: to.0, y: to.1, delay_ms: STEP_MS });
    out
}

/// A touch held in place for `ms` before release: a `begin` immediately
/// followed by an `end` after the hold. No intermediate `move` steps, since
/// the finger doesn't travel — this is what tells iOS it's a long-press and
/// not a drag.
pub fn long_press(x: f64, y: f64, ms: u64) -> Vec<TouchStep> {
    let (x, y) = clamp(x, y);
    vec![
        TouchStep { phase: TouchPhase::Begin, x, y, delay_ms: 0 },
        TouchStep { phase: TouchPhase::End, x, y, delay_ms: ms },
    ]
}

/// Keep a wheel-driven finger this far inside the screen: nearer the edge
/// it lifts and starts again at the anchor (upstream `scrollEdgeMargin`).
pub const WHEEL_EDGE_MARGIN: f64 = 0.08;

/// Wheel travel (display-normalized) before a finger goes down: a finger
/// that lands and lifts within iOS's tap slop (~10 pt) would be a tap.
pub const WHEEL_SLOP: f64 = 0.015;

/// Wheel / trackpad scrolling as a one-finger drag — what upstream's native
/// `SimHID.scroll` does (`HIDInjector.swift` `sendScroll`), done here because
/// helper v0.2.0 drops `scroll` (it passes a zero screen size, which
/// upstream's guard rejects).
///
/// Once the deltas add up to [`WHEEL_SLOP`], a finger goes down under the
/// cursor (so iOS hit-tests the scroll view there), each delta moves it, and
/// [`WheelDrag::finish`] lifts
/// it once the wheel goes idle. A finger that would leave the margin lifts
/// and starts again at the anchor, so a long scroll keeps going. Points and
/// deltas are **display**-normalized, in the finger's direction of travel
/// (content follows the finger); the caller maps each point to portrait.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WheelDrag {
    /// `(anchor, finger)` while a finger is down.
    down: Option<((f64, f64), (f64, f64))>,
    /// Travel so far, before a finger is down.
    pending: (f64, f64),
}

impl WheelDrag {
    pub fn is_active(&self) -> bool {
        self.down.is_some()
    }

    /// Steps for one wheel delta at `cursor`. Delays are 0: the caller sends
    /// them as the deltas arrive.
    pub fn scroll(&mut self, cursor: (f64, f64), delta: (f64, f64)) -> Vec<TouchStep> {
        let step = |phase, (x, y): (f64, f64)| TouchStep { phase, x, y, delay_ms: 0 };
        let mut out = Vec::new();
        let (anchor, finger, delta) = match self.down {
            Some((anchor, finger)) => (anchor, finger, delta),
            None => {
                let travel = (self.pending.0 + delta.0, self.pending.1 + delta.1);
                if travel.0.hypot(travel.1) < WHEEL_SLOP {
                    self.pending = travel;
                    return out;
                }
                self.pending = (0.0, 0.0);
                let anchor = inside(cursor);
                out.push(step(TouchPhase::Begin, anchor));
                (anchor, anchor, travel)
            }
        };
        let mut next = (finger.0 + delta.0, finger.1 + delta.1);
        if !within_margin(next) {
            out.push(step(TouchPhase::End, finger));
            out.push(step(TouchPhase::Begin, anchor));
            next = (anchor.0 + delta.0, anchor.1 + delta.1);
        }
        let next = inside(next);
        out.push(step(TouchPhase::Move, next));
        self.down = Some((anchor, next));
        out
    }

    /// Lift the finger (the wheel went idle, or a click took over). Travel
    /// that never reached the slop is forgotten.
    pub fn finish(&mut self) -> Option<TouchStep> {
        self.pending = (0.0, 0.0);
        let (_, (x, y)) = self.down.take()?;
        Some(TouchStep { phase: TouchPhase::End, x, y, delay_ms: 0 })
    }
}

fn within_margin((x, y): (f64, f64)) -> bool {
    let ok = |v: f64| (WHEEL_EDGE_MARGIN..=1.0 - WHEEL_EDGE_MARGIN).contains(&v);
    ok(x) && ok(y)
}

fn inside((x, y): (f64, f64)) -> (f64, f64) {
    let c = |v: f64| v.clamp(WHEEL_EDGE_MARGIN, 1.0 - WHEEL_EDGE_MARGIN);
    (c(x), c(y))
}

/// The second finger of an Option-drag pinch: `p` mirrored through `center`,
/// clamped to the screen.
pub fn mirror(p: (f64, f64), center: (f64, f64)) -> (f64, f64) {
    clamp(2.0 * center.0 - p.0, 2.0 * center.1 - p.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_is_begin_then_end_at_the_same_point() {
        let steps = tap(0.5, 0.5);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], TouchStep { phase: TouchPhase::Begin, x: 0.5, y: 0.5, delay_ms: 0 });
        assert_eq!(steps[1], TouchStep { phase: TouchPhase::End, x: 0.5, y: 0.5, delay_ms: STEP_MS });
    }

    #[test]
    fn tap_clamps_out_of_range_coordinates() {
        let steps = tap(-1.0, 2.0);
        assert_eq!((steps[0].x, steps[0].y), (0.0, 1.0));
    }

    #[test]
    fn swipe_steps_every_16ms_between_begin_and_end() {
        let steps = swipe((0.0, 0.0), (1.0, 0.0), 64);
        // 64 / 16 = 4 steps: begin + 3 moves + end.
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[0].phase, TouchPhase::Begin);
        assert!(steps[1..4].iter().all(|s| s.phase == TouchPhase::Move));
        assert_eq!(steps[4].phase, TouchPhase::End);
        assert_eq!(steps[4].x, 1.0);
        // Linear interpolation along x.
        assert!((steps[1].x - 0.25).abs() < 1e-9);
        assert!((steps[2].x - 0.5).abs() < 1e-9);
        assert!((steps[3].x - 0.75).abs() < 1e-9);
        // Every step after the first waits one tick.
        assert!(steps[1..].iter().all(|s| s.delay_ms == STEP_MS));
        assert_eq!(steps[0].delay_ms, 0);
    }

    #[test]
    fn swipe_shorter_than_one_step_still_moves_once() {
        let steps = swipe((0.0, 0.0), (1.0, 1.0), 5);
        assert_eq!(steps.len(), 2, "begin + end, no intermediate move fits in 5ms");
        assert_eq!(steps[0].phase, TouchPhase::Begin);
        assert_eq!(steps[1].phase, TouchPhase::End);
        assert_eq!((steps[1].x, steps[1].y), (1.0, 1.0));
    }

    #[test]
    fn swipe_clamps_endpoints() {
        let steps = swipe((-0.5, 0.5), (1.5, 0.5), 16);
        assert_eq!(steps[0].x, 0.0);
        assert_eq!(steps.last().unwrap().x, 1.0);
    }

    #[test]
    fn long_press_has_no_intermediate_moves() {
        let steps = long_press(0.2, 0.8, 750);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], TouchStep { phase: TouchPhase::Begin, x: 0.2, y: 0.8, delay_ms: 0 });
        assert_eq!(steps[1], TouchStep { phase: TouchPhase::End, x: 0.2, y: 0.8, delay_ms: 750 });
    }

    fn phases(steps: &[TouchStep]) -> Vec<TouchPhase> {
        steps.iter().map(|s| s.phase).collect()
    }

    #[test]
    fn wheel_drag_puts_a_finger_down_under_the_cursor_then_moves_it() {
        let mut drag = WheelDrag::default();
        let steps = drag.scroll((0.5, 0.5), (0.0, -0.1));
        assert_eq!(phases(&steps), [TouchPhase::Begin, TouchPhase::Move]);
        assert_eq!((steps[0].x, steps[0].y), (0.5, 0.5));
        assert!((steps[1].y - 0.4).abs() < 1e-9);
        // Later deltas only move the finger that is already down.
        let steps = drag.scroll((0.9, 0.9), (0.0, -0.1));
        assert_eq!(phases(&steps), [TouchPhase::Move]);
        assert!((steps[0].y - 0.3).abs() < 1e-9 && steps[0].x == 0.5);
        let end = drag.finish().unwrap();
        assert_eq!(end.phase, TouchPhase::End);
        assert!((end.y - 0.3).abs() < 1e-9);
        assert!(drag.finish().is_none() && !drag.is_active());
    }

    #[test]
    fn wheel_drag_reanchors_before_leaving_the_margin() {
        let mut drag = WheelDrag::default();
        drag.scroll((0.5, 0.5), (0.0, -0.3));
        let steps = drag.scroll((0.5, 0.5), (0.0, -0.3));
        assert_eq!(phases(&steps), [TouchPhase::End, TouchPhase::Begin, TouchPhase::Move]);
        assert_eq!((steps[1].x, steps[1].y), (0.5, 0.5), "starts again at the anchor");
        assert!((steps[2].y - 0.2).abs() < 1e-9);
    }

    #[test]
    fn wheel_drag_keeps_its_finger_inside_the_margin() {
        let mut drag = WheelDrag::default();
        let steps = drag.scroll((0.0, 1.0), (0.0, -0.05));
        assert_eq!((steps[0].x, steps[0].y), (WHEEL_EDGE_MARGIN, 1.0 - WHEEL_EDGE_MARGIN));
        // A delta bigger than the whole screen still lands inside.
        let steps = drag.scroll((0.5, 0.5), (5.0, 0.0));
        let last = steps.last().unwrap();
        assert!(last.x <= 1.0 - WHEEL_EDGE_MARGIN && last.x >= WHEEL_EDGE_MARGIN);
    }

    #[test]
    fn wheel_drag_waits_for_the_slop_so_a_nudge_is_never_a_tap() {
        let mut drag = WheelDrag::default();
        assert!(drag.scroll((0.5, 0.5), (0.0, 0.005)).is_empty());
        assert!(drag.finish().is_none(), "nothing went down");
        assert!(drag.scroll((0.5, 0.5), (0.0, 0.01)).is_empty());
        let steps = drag.scroll((0.5, 0.5), (0.0, 0.01));
        assert_eq!(phases(&steps), [TouchPhase::Begin, TouchPhase::Move]);
        assert!((steps[1].y - 0.52).abs() < 1e-9, "the held-back travel is applied");
    }

    #[test]
    fn mirror_reflects_through_the_center_and_clamps() {
        assert_eq!(mirror((0.3, 0.4), (0.5, 0.5)), (0.7, 0.6));
        assert_eq!(mirror((0.1, 0.5), (0.8, 0.5)), (1.0, 0.5));
    }
}
