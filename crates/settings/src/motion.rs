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

impl Global for Motion {}
