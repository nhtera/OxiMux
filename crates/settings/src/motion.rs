//! Motion tokens — animation durations + the easing contract.
//!
//! Source of truth: `docs/design-guidelines.md` ("## Motion").
//!
//! Doctrine: bake animated values **once per state change**, never per frame
//! (a per-frame rebuild that re-creates the animation each tick re-arms it and
//! pins the CPU). Every duration is sub-200ms so motion reads as
//! responsiveness, not lag. Easing is `gpui::ease_out_quint()` at the call
//! site — it lands close to the reference `cubic-bezier(0.16, 1, 0.3, 1)`
//! "ease-out-expo-ish" curve: fast out of the gate, gentle settle.
//!
//! Held as a GPUI [`Global`] so the reduced-motion preference is a single
//! switch: startup picks [`Motion::cockpit`] or [`Motion::reduced`] and every
//! call site reads the same value. Tokens are plain `Copy`, so a render fn can
//! snapshot the global and thread it into a pure layout fn (mirroring how
//! `Density` / `Typography` are passed).

use std::time::Duration;

#[cfg(feature = "gpui")]
use gpui::Global;

/// Animation durations for the disciplined sub-200ms motion vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    /// Hover-state cross-fade (bg/fg). Only used where hover animation is
    /// affordable without per-row state churn — see the design-guidelines
    /// note on GPUI's instant `.hover()` swap.
    pub m_hover: Duration,
    /// Overlay / picker / modal enter (fade + 0.98→1.0 scale). The
    /// command-palette open is the headline surface.
    pub m_overlay: Duration,
    /// Collapsible section expand / collapse (height + opacity).
    pub m_collapse: Duration,
    /// Toast enter (slide/fade in).
    pub m_toast_in: Duration,
    /// Toast exit (fade out) — quicker than enter so dismissal feels crisp.
    pub m_toast_out: Duration,
    /// Exit/dismiss duration for surfaces adopting the accelerate-out
    /// [`ease_in_exit`] curve (the exit counterpart of `ease_out_spring`).
    /// Surfaces not yet converted keep their existing close timings.
    pub m_exit: Duration,
    /// True when this is the reduced-motion variant (durations collapsed to an
    /// instant floor). Lets a call site branch cheaply if it wants to skip the
    /// animation wrapper entirely rather than play a 1ms no-op.
    pub reduced: bool,
}

impl Motion {
    /// Standard cockpit motion. The only non-reduced variant in v1.
    pub fn cockpit() -> Self {
        Self {
            m_hover: Duration::from_millis(120),
            m_overlay: Duration::from_millis(180),
            m_collapse: Duration::from_millis(190),
            m_toast_in: Duration::from_millis(180),
            m_toast_out: Duration::from_millis(140),
            m_exit: Duration::from_millis(200),
            reduced: false,
        }
    }

    /// Reduced-motion variant — every duration collapses to a 1ms floor.
    ///
    /// 1ms (not `Duration::ZERO`) on purpose: GPUI advances an animation by
    /// `elapsed / duration`, so a zero duration risks a divide-by-zero / NaN
    /// delta. 1ms completes inside the first frame, so the animation lands on
    /// its end state immediately — visually instant, numerically safe.
    pub fn reduced() -> Self {
        let floor = Duration::from_millis(1);
        Self {
            m_hover: floor,
            m_overlay: floor,
            m_collapse: floor,
            m_toast_in: floor,
            m_toast_out: floor,
            m_exit: floor,
            reduced: true,
        }
    }

    /// Pick the variant for a reduced-motion preference flag.
    pub fn resolve(reduce: bool) -> Self {
        if reduce {
            Self::reduced()
        } else {
            Self::cockpit()
        }
    }
}

impl Default for Motion {
    fn default() -> Self {
        Self::cockpit()
    }
}

#[cfg(feature = "gpui")]
impl Global for Motion {}

/// Spring-tail easing for overlay/palette OPEN animations only —
/// cubic-bezier(0.16, 1, 0.3, 1): faster out of the gate than
/// `ease_out_quint` with a longer deceleration tail, so an opening
/// surface reads "snapped into place, then settled". Closes and every
/// other animation keep `gpui::ease_out_quint()` — the vocabulary split
/// lives in `docs/design-guidelines.md` ("## Motion").
pub fn ease_out_spring() -> impl Fn(f32) -> f32 {
    |x| cubic_bezier_ease(0.16, 1.0, 0.3, 1.0, x)
}

/// Accelerate-out easing for EXIT animations — cubic-bezier(0.7, 0, 0.84, 0):
/// a closing surface lingers for a beat then accelerates away, the mirror
/// image of `ease_out_spring`'s fast-start settle. Paired with `m_exit`.
/// Surfaces not yet converted keep `gpui::ease_out_quint()` on close.
pub fn ease_in_exit() -> impl Fn(f32) -> f32 {
    |x| cubic_bezier_ease(0.7, 0.0, 0.84, 0.0, x)
}

/// Evaluate a CSS-style cubic-bezier timing curve at progress `x` (both
/// axes 0..=1). Control x-coordinates within [0, 1] make x(t) monotonic,
/// so a bisection solve for t is safe; ~20 iterations lands well under
/// one output pixel of error for sub-200ms animations.
fn cubic_bezier_ease(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let sample = |c1: f32, c2: f32, t: f32| {
        let omt = 1.0 - t;
        3.0 * omt * omt * t * c1 + 3.0 * omt * t * t * c2 + t * t * t
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut t = x;
    for _ in 0..20 {
        if sample(x1, x2, t) < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) * 0.5;
    }
    sample(y1, y2, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_ease_endpoints_pin_exactly() {
        let f = ease_out_spring();
        assert_eq!(f(0.0), 0.0);
        assert_eq!(f(1.0), 1.0);
    }

    #[test]
    fn spring_ease_is_monotonic_and_never_overshoots() {
        // Position-critical layouts ride this curve — it must never go
        // above 1.0 (no bounce) and never move backwards.
        let f = ease_out_spring();
        let mut prev = 0.0f32;
        for i in 0..=100 {
            let y = f(i as f32 / 100.0);
            assert!(y >= prev - 1e-4, "regressed at step {i}: {y} < {prev}");
            assert!((0.0..=1.0 + 1e-4).contains(&y), "out of range at {i}: {y}");
            prev = y;
        }
    }

    #[test]
    fn exit_ease_endpoints_pin_exactly() {
        let f = ease_in_exit();
        assert_eq!(f(0.0), 0.0);
        assert_eq!(f(1.0), 1.0);
    }

    #[test]
    fn exit_ease_is_monotonic_and_back_loads_motion() {
        // Exit reads "linger, then accelerate away": well under half the
        // distance at 50% time, monotonic, never out of range.
        let f = ease_in_exit();
        let mut prev = 0.0f32;
        for i in 0..=100 {
            let y = f(i as f32 / 100.0);
            assert!(y >= prev - 1e-4, "regressed at step {i}: {y} < {prev}");
            assert!((0.0..=1.0 + 1e-4).contains(&y), "out of range at {i}: {y}");
            prev = y;
        }
        assert!(f(0.5) < 0.30, "not back-loaded: f(0.5) = {}", f(0.5));
    }

    #[test]
    fn spring_ease_front_loads_motion() {
        // The whole point of the curve: most of the travel happens early
        // (fast start), the tail settles. At 20% time it must be well past
        // half the distance, and it must sit above ease-out-quint's value
        // early on.
        let f = ease_out_spring();
        assert!(f(0.2) > 0.55, "not front-loaded: f(0.2) = {}", f(0.2));
        assert!(f(0.5) > 0.85, "tail not settling: f(0.5) = {}", f(0.5));
    }
}
