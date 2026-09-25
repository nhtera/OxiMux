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
}
