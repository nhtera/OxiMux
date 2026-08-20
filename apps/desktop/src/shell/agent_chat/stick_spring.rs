//! The glide that keeps a streaming transcript's tail on screen.
//!
//! # What this replaces, and what it does not
//!
//! Not the follow *state machine* — `gpui::list` owns that, and owns it well.
//! `FollowMode::Tail` re-pins to the end on every layout, drops the pin when the
//! user wheels **up** (`list.rs`, `if delta.y > px(0.)`) rather than when the
//! scroll position happens to leave the bottom, and picks it back up when the
//! position returns to the end. That last distinction is the one the whole class
//! of follow bugs turns on: a controller that breaks its own pin by scrolling is
//! a controller that cannot follow. gpui already gets it right.
//!
//! What `Tail` does not do is *travel*. It assigns the end position, so every
//! repaint carrying new text lands the view somewhere new instantly. At the
//! transcript's 50ms delta cadence that is twenty discrete jumps a second, each
//! as tall as whatever arrived — a paragraph, a fence, a tool card. This module
//! spends those 50ms moving instead.
//!
//! The trade is explicit: to glide, the list runs in `FollowMode::Normal` and
//! this side owns engagement, disengagement and re-engagement.
//!
//! # Lag, not position
//!
//! The obvious spring drives an absolute scroll position toward an absolute end.
//! In a `list()` neither number is trustworthy. A row the list has never laid
//! out is `ListItem::Unmeasured` and contributes **zero** height, so on a
//! restored transcript the content measures as nearly nothing: the view is at
//! the top, and every pixel-denominated question — "how far to the bottom" above
//! all — answers zero. A spring told the distance is zero never moves. That was
//! measured, not feared; an earlier build of this module sat 464px short of the
//! end believing it had arrived.
//!
//! So the spring holds **lag**: how far behind the end the view is deliberately
//! being held, in pixels, and nothing else. Each frame the caller re-anchors to
//! the end by *item index* (`scroll_to_end`, which the layout resolves by
//! walking backwards and which is therefore exact with nothing measured at all),
//! then scrolls back by the current lag. Content arriving adds to the lag; the
//! spring works it back down to zero.
//!
//! Two things fall out of that, both of which the position formulation had to
//! fight for:
//!
//! - **No accumulation.** Every frame's position is derived afresh from an exact
//!   anchor, so a rounding error or a dropped frame cannot leave the view
//!   permanently short.
//! - **No ambiguity about growth.** The caller adds growth to the lag itself, so
//!   the spring never has to work out which part of a change was content moving
//!   and which was its own travel.
//!
//! # Attribution
//!
//! The velocity/feed-forward shape and the starting constants are derived from
//! an MIT-licensed reference implementation; see `THIRD_PARTY_NOTICES.md`. The
//! lag formulation and the grace handling are not — they are what it took to sit
//! on top of `gpui::list` rather than a plain scroll container.

/// Velocity retained frame to frame.
const DAMPING: f32 = 0.7;
/// Pull toward the goal, per frame.
const STIFFNESS: f32 = 0.05;
/// Inertia. With the three together the steady-state velocity settles at
/// `0.05/(1.25 - 0.7) ≈ 9%` of the remaining lag per frame — a time constant
/// near 11 frames, ~180ms, which is a glide rather than a drift.
const MASS: f32 = 1.25;

/// The reference frame length the constants above are expressed in.
const FRAME_MS: f32 = 1000.0 / 60.0;

/// Most sub-frames one tick may integrate. A hitch (a GC pause, a window drag, a
/// tab the compositor stopped painting) must catch up, not teleport; past this
/// the spring moves as far as eight frames would and takes the rest next tick.
const MAX_CATCHUP_FRAMES: f32 = 8.0;

/// Weight of the newest growth sample in the feed-forward estimate.
const GROWTH_EMA: f32 = 0.12;

/// Lag closer than this is not worth animating; close it in one step so the tail
/// does not creep toward the edge forever in sub-pixel increments.
const SNAP_PX: f32 = 0.5;

/// How long after the last motion the spring keeps its speed, so a stream that
/// pauses for a beat resumes at cruise instead of re-accelerating from a
/// standstill.
const SETTLE_GRACE_FRAMES: f32 = 500.0 / FRAME_MS;

/// How close to the end the user must scroll back for the pin to re-engage.
///
/// `gpui::list`'s own `Tail` re-engages at one pixel — exactly the end and
/// nothing less. That is right for a log viewer and wrong for a conversation:
/// finishing a reply and flicking down leaves you a hair short, and a hair short
/// meant the next turn did not follow. A band deep enough to mean "you are
/// reading the live edge" is the whole difference.
pub(super) const STICK_THRESHOLD_PX: f32 = 70.0;

/// Furthest behind the end the spring will ever glide from. A jump longer than
/// this is not a glide anybody wants to sit through, so the caller lands the
/// excess instantly and the spring animates the last screen.
pub(super) const GLIDE_MAX_LAG_VIEWPORTS: f32 = 1.0;

/// Convert an elapsed wall-clock duration into the spring's sub-frame budget.
pub(super) fn frames_elapsed(dt: std::time::Duration) -> f32 {
    dt.as_secs_f32() * 1000.0 / FRAME_MS
}

/// A velocity spring that closes a lag behind a moving end.
///
/// Pure: no GPUI, no clock, no interior mutability. Every input arrives as an
/// argument to [`step`](Self::step) and the only output is how much of the lag
/// to close.
#[derive(Debug, Default)]
pub(super) struct StickSpring {
    /// Current speed, px per 60fps frame.
    vel: f32,
    /// Feed-forward: an EMA of how fast the end is running away, px per frame.
    /// Without it the spring is always a little behind live text, since it only
    /// ever reacts to a lag that has already opened.
    growth_vel: f32,
    /// Frames of settle grace left before the motion memory is abandoned.
    warm: f32,
}

impl StickSpring {
    /// Forget all motion. For releasing the pin, and for re-engaging it from a
    /// standstill after the user has been reading elsewhere.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Close some of `lag`, given that `growth` pixels of it arrived this tick,
    /// and return the lag that remains.
    ///
    /// `frames` is how many 60fps sub-frames of wall clock this tick covers.
    pub fn step(&mut self, lag: f32, growth: f32, frames: f32) -> f32 {
        // Grace drains against the *unclamped* elapsed time. A tick arriving
        // after a long quiet gap should find the spring cold, even though the
        // integration below refuses to simulate more than eight frames of it.
        self.warm = (self.warm - frames).max(0.0);
        if self.warm == 0.0 {
            // Cold: both halves of the motion memory go. A growth estimate from
            // a stream that stopped a second ago is a claim about a stream that
            // no longer exists.
            self.vel = 0.0;
            self.growth_vel = 0.0;
        }
        if growth > 0.0 && frames > 0.0 {
            let per_frame = growth / frames;
            self.growth_vel += (per_frame - self.growth_vel) * GROWTH_EMA;
        } else if growth < 0.0 {
            // The end came back toward us — a tool card collapsed, a rewind
            // dropped rows. A stale positive estimate is now feed-forward
            // pushing at a wall that has moved, so drop it rather than decay it.
            self.growth_vel = 0.0;
        }

        let travelled = self.travel(lag, frames);
        if growth != 0.0 || travelled > 0.0 {
            // Any motion at all keeps the spring warm; the grace is about a
            // genuinely *quiet* stretch. An earlier spelling refreshed only on
            // arrival and so went cold in the middle of a stream that had simply
            // not caught up yet, taking the growth estimate with it.
            self.warm = SETTLE_GRACE_FRAMES;
        }
        (lag - travelled).max(0.0)
    }

    /// How far to move this tick. Never negative, never more than the lag.
    fn travel(&mut self, lag: f32, frames: f32) -> f32 {
        if lag <= SNAP_PX {
            // `vel` deliberately survives arriving: zeroing it here meant every
            // landing went cold instantly and the settle grace above could never
            // preserve anything. Whether the speed lives is the grace's call.
            return lag.max(0.0);
        }
        let mut budget = frames.clamp(0.0, MAX_CATCHUP_FRAMES);
        let mut gap = lag;
        let mut travelled = 0.0;
        while budget > 0.0 {
            let sub = budget.min(1.0);
            self.vel = (DAMPING * self.vel + STIFFNESS * gap) / MASS;
            let d = (self.vel + self.growth_vel) * sub;
            travelled += d;
            gap -= d;
            budget -= sub;
        }
        travelled.clamp(0.0, lag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tick's worth of frames, the cadence the transcript actually repaints
    /// streamed deltas at (`NOTIFY_INTERVAL`, 50ms).
    const TICK: f32 = 50.0 / FRAME_MS;

    /// Close a fixed lag with nothing arriving, returning every lag visited.
    fn glide(spring: &mut StickSpring, mut lag: f32, ticks: usize) -> Vec<f32> {
        (0..ticks)
            .map(|_| {
                lag = spring.step(lag, 0.0, TICK);
                lag
            })
            .collect()
    }

    /// Stream `growth` px a tick and return the lag left at the end.
    fn stream(spring: &mut StickSpring, growth: f32, ticks: usize) -> f32 {
        let mut lag = 0.0;
        for _ in 0..ticks {
            lag = spring.step(lag + growth, growth, TICK);
        }
        lag
    }

    /// Monotone approach, and arrival.
    ///
    /// Overshoot is **not** asserted here, because in this formulation it cannot
    /// happen: the view is placed at `end - lag` with the lag floored at zero,
    /// so there is no arrangement of velocity, growth and elapsed time that puts
    /// it past the end. Deliberately breaking [`StickSpring::travel`]'s own
    /// clamp leaves every assertion below still passing, which is the honest
    /// signal that the property is structural rather than guarded — an
    /// assertion that cannot fail is worse than no assertion, because it reads
    /// like coverage.
    #[test]
    fn the_approach_is_monotone_and_arrives() {
        let mut s = StickSpring::default();
        let seen = glide(&mut s, 400.0, 60);
        for pair in seen.windows(2) {
            assert!(pair[1] <= pair[0], "the lag grew with nothing arriving: {pair:?}");
        }
        assert!(seen.last().unwrap() < &SNAP_PX, "never arrived: {seen:?}");
    }

    #[test]
    fn it_glides_rather_than_snapping() {
        // The whole point of the phase: the first tick closes some of the lag,
        // not all of it. A regression to assign-the-end reads as 0.0 here.
        let mut s = StickSpring::default();
        let left = s.step(400.0, 400.0, TICK);
        assert!(left < 399.0, "did not move at all: {left} left of 400");
        assert!(left > 200.0, "snapped instead of gliding: {left} left of 400");
    }

    #[test]
    fn the_last_half_pixel_is_closed_in_one_step() {
        let mut s = StickSpring::default();
        assert_eq!(s.step(0.4, 0.0, TICK), 0.0);
    }

    #[test]
    fn feed_forward_keeps_up_with_steady_growth() {
        // A spring with no feed-forward settles at a standing lag proportional
        // to the growth rate; this asserts the lag closes instead.
        let mut s = StickSpring::default();
        let lag = stream(&mut s, 25.0, 80);
        assert!(lag < 25.0, "fell {lag}px behind live text and stayed there");
    }

    #[test]
    fn a_shrinking_list_zeroes_the_growth_estimate() {
        let mut s = StickSpring::default();
        stream(&mut s, 30.0, 40);
        assert!(s.growth_vel > 0.0, "no estimate to lose");
        // A tool card collapses: 200px of content gone.
        s.step(0.0, -200.0, TICK);
        assert_eq!(s.growth_vel, 0.0, "kept a stale estimate after the end moved back");
    }

    #[test]
    fn a_hitch_catches_up_without_teleporting() {
        // A one-second stall arrives as 60 frames; the spring simulates 8.
        let after_hitch = StickSpring::default().step(400.0, 0.0, 60.0);
        let bounded = StickSpring::default().step(400.0, 0.0, MAX_CATCHUP_FRAMES);
        assert_eq!(after_hitch, bounded);
        assert!(after_hitch > 0.0, "teleported");
    }

    #[test]
    fn it_parks_when_there_is_nothing_to_chase() {
        let mut s = StickSpring::default();
        let seen = glide(&mut s, 400.0, 120);
        assert_eq!(*seen.last().unwrap(), 0.0, "never reached zero lag: {s:?}");
    }

    /// Land, wait `pause` frames, then face a fresh 200px lag. Returns how much
    /// of it the first tick closes.
    fn resume_after(pause: f32) -> f32 {
        let mut s = StickSpring::default();
        stream(&mut s, 30.0, 40);
        s.step(0.0, 0.0, pause);
        200.0 - s.step(200.0, 200.0, TICK)
    }

    #[test]
    fn a_brief_pause_resumes_at_cruise_and_a_long_one_does_not() {
        // The grace's entire contract, and the only framing that observes it.
        // Both resumptions face an identical lag and an identical growth
        // sample, so the difference between them is purely the velocity the
        // shorter pause kept.
        let brief = resume_after(6.0);
        let long = resume_after(300.0);
        assert!(
            brief > long * 1.1,
            "a 100ms pause closed {brief}px, no more than the {long}px after five seconds"
        );
    }
}
