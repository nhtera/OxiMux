//! Automatic retry of a turn that failed on a provider limit.
//!
//! Split so the part that is easy to get wrong is testable without a clock:
//! [`classify`] turns typed wire signals into a [`RetryClass`], and
//! [`schedule`] is a pure function from `(class, attempt, now, settings, seed)`
//! to a wake time. Nothing here starts a timer or touches a thread.
//!
//! ## Why this does not reuse the schedule ticker
//!
//! The plan called for reusing `crate::schedule`. It cannot be reused, for four
//! independent reasons, each of which alone is disqualifying:
//!
//! 1. `Recurrence` has no one-shot variant and enforces a five-minute floor at
//!    construction — a retry is a single fire, often sooner than that.
//! 2. A `Schedule` is a user-visible row with a name and an enabled flag,
//!    listed by the Schedules UI and `oximux schedule ls`, and carried on the
//!    remote wire as `RecurrenceWire`. Internal retries do not belong there,
//!    and the wire type is frozen.
//! 3. The ticker runs under a single per-data-dir role lock, so it fires in
//!    whichever host holds that lock. A retry must fire in the process that
//!    owns the thread, which is not necessarily the same one.
//! 4. `nudge_existing_session` deliberately wraps its prompt in a preamble
//!    telling the agent a timer armed it. A retry must resend the user's own
//!    prompt verbatim.
//!
//! What is reused is the *shape*: a pure time-arithmetic module tested without
//! a database, matching `schedule::recurrence`.

pub mod classify;

pub use classify::{RetryClass, classify_failure};

use std::time::Duration;

/// Most automatic attempts for one turn.
///
/// Four is the cap the user cannot raise. A window retry consumes one attempt
/// per reset, so four covers roughly a day of five-hour windows — past that the
/// account is not going to free up on its own and a person should look.
pub const MAX_ATTEMPTS: u32 = 4;

/// First overload backoff, doubled per attempt: 30s, 60s, 120s, 240s.
pub const OVERLOAD_BASE: Duration = Duration::from_secs(30);

/// Added after a window's reported reset before retrying.
///
/// Provider clocks and ours disagree by seconds, and a request landing one
/// second early is refused and burns an attempt. Waiting a little past the
/// reset costs nothing.
pub const WINDOW_GRACE: Duration = Duration::from_secs(30);

/// Width of the random spread added to every wake time.
///
/// **The point of the feature, not a refinement.** Every thread on one account
/// sees the same reset time, so without a spread they all wake in the same
/// second, and the account re-limits itself instantly — the retry storms the
/// thing it exists to survive. Two minutes is wide enough to spread a realistic
/// number of threads across distinct seconds and short enough that a user
/// watching a countdown does not notice.
pub const JITTER_SPAN: Duration = Duration::from_secs(120);

/// User-facing retry configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    /// Longest the app will wait automatically. `None` means no limit.
    ///
    /// A weekly window that resets in six days is technically retryable, but
    /// silently holding a turn for six days is worse than reporting the error —
    /// the user cannot tell a queued turn from a forgotten one.
    pub max_automatic_wait: Option<Duration>,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self { max_automatic_wait: Some(Duration::from_secs(24 * 60 * 60)) }
    }
}

/// When to retry, or `None` to surface the failure to the user.
///
/// Pure: no clock, no randomness of its own. `now_ms` and `seed` are supplied
/// so the cap, the ceiling, and the jitter *distribution* can all be tested
/// without waiting or flaking.
pub fn schedule(
    class: RetryClass,
    attempt: u32,
    now_ms: i64,
    settings: &RetrySettings,
    seed: u64,
) -> Option<i64> {
    if attempt >= MAX_ATTEMPTS || !class.is_retryable() {
        return None;
    }
    let jitter = jitter_ms(seed, JITTER_SPAN.as_millis() as u64);
    let wake_ms = match class {
        RetryClass::Overload => {
            // Doubling from the base: attempt 0 waits 30s, attempt 3 waits 240s.
            let backoff = OVERLOAD_BASE.as_millis() as u64 * (1u64 << attempt);
            now_ms.checked_add((backoff + jitter) as i64)?
        }
        RetryClass::Window { resets_at_ms } => {
            // A reset already in the past means the window reopened while the
            // failure was in flight; wait from now rather than firing instantly
            // into an account that may still be catching up.
            let base = resets_at_ms.max(now_ms);
            base.checked_add((WINDOW_GRACE.as_millis() as u64 + jitter) as i64)?
        }
        RetryClass::Spend | RetryClass::Other => return None,
    };
    if let Some(limit) = settings.max_automatic_wait {
        let waited_ms = wake_ms.saturating_sub(now_ms);
        if waited_ms > limit.as_millis() as i64 {
            return None;
        }
    }
    Some(wake_ms)
}

/// A uniformly spread offset in `[0, span_ms)` derived from `seed`.
///
/// SplitMix64's finalizer: it avalanches, so seeds that differ in one bit —
/// consecutive thread ids, say — produce unrelated offsets. A weaker mix (`seed
/// % span`) would leave sequential ids landing in sequential milliseconds,
/// which is the clustering this exists to prevent.
fn jitter_ms(seed: u64, span_ms: u64) -> u64 {
    if span_ms == 0 {
        return 0;
    }
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) % span_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const NOW: i64 = 1_788_000_000_000;

    fn unlimited() -> RetrySettings {
        RetrySettings { max_automatic_wait: None }
    }

    #[test]
    fn overload_backoff_doubles_per_attempt() {
        // Compare the jitter-free floor of each attempt: with one seed the
        // jitter is a constant, so the gaps are the backoff.
        let waits: Vec<i64> = (0..MAX_ATTEMPTS)
            .map(|a| schedule(RetryClass::Overload, a, NOW, &unlimited(), 7).unwrap() - NOW)
            .collect();
        for pair in waits.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            let jitter = waits[0] - 30_000;
            assert_eq!(next - jitter, (prev - jitter) * 2, "each attempt doubles: {waits:?}");
        }
    }

    #[test]
    fn a_window_is_scheduled_past_its_reset_never_before() {
        let resets_at_ms = NOW + 6 * 60 * 60 * 1000;
        for seed in 0..200 {
            let wake =
                schedule(RetryClass::Window { resets_at_ms }, 0, NOW, &unlimited(), seed).unwrap();
            assert!(wake >= resets_at_ms + WINDOW_GRACE.as_millis() as i64, "seed {seed}");
        }
    }

    /// A reset that has already passed must not fire instantly — the window
    /// reopened while the failure was in flight, and the account may still be
    /// catching up.
    #[test]
    fn a_reset_already_in_the_past_waits_from_now() {
        let wake =
            schedule(RetryClass::Window { resets_at_ms: NOW - 60_000 }, 0, NOW, &unlimited(), 3)
                .unwrap();
        assert!(wake >= NOW + WINDOW_GRACE.as_millis() as i64);
    }

    /// The test the feature lives or dies on. An implementation returning a
    /// constant offset passes "jitter is non-zero" and still wakes every thread
    /// on the account in the same second.
    #[test]
    fn twenty_threads_against_one_reset_get_twenty_distinct_wake_times() {
        let resets_at_ms = NOW + 5 * 60 * 60 * 1000;
        let wakes: Vec<i64> = (0..20)
            .map(|thread| {
                schedule(RetryClass::Window { resets_at_ms }, 0, NOW, &unlimited(), thread).unwrap()
            })
            .collect();
        let distinct: HashSet<i64> = wakes.iter().copied().collect();
        assert_eq!(distinct.len(), 20, "every thread must wake at its own moment: {wakes:?}");

        // And they must actually *spread*, not merely differ: 20 samples over a
        // 120s span should not all huddle inside one second.
        let spread = wakes.iter().max().unwrap() - wakes.iter().min().unwrap();
        assert!(spread > 30_000, "wake times cluster in {spread}ms, jitter is not spreading");
    }

    #[test]
    fn jitter_stays_inside_its_span() {
        for seed in 0..10_000 {
            assert!(jitter_ms(seed, 120_000) < 120_000);
        }
        assert_eq!(jitter_ms(42, 0), 0, "a zero span must not divide by zero");
    }

    #[test]
    fn the_cap_is_four_attempts() {
        assert!(schedule(RetryClass::Overload, MAX_ATTEMPTS - 1, NOW, &unlimited(), 1).is_some());
        assert!(schedule(RetryClass::Overload, MAX_ATTEMPTS, NOW, &unlimited(), 1).is_none());
        assert!(schedule(RetryClass::Overload, 99, NOW, &unlimited(), 1).is_none());
    }

    #[test]
    fn spend_and_other_are_never_scheduled() {
        for class in [RetryClass::Spend, RetryClass::Other] {
            for attempt in 0..MAX_ATTEMPTS {
                assert!(
                    schedule(class, attempt, NOW, &unlimited(), 1).is_none(),
                    "{class:?} attempt {attempt} must not schedule"
                );
            }
        }
    }

    #[test]
    fn a_reset_beyond_the_ceiling_is_not_scheduled() {
        let settings = RetrySettings { max_automatic_wait: Some(Duration::from_secs(6 * 60 * 60)) };
        let within = NOW + 5 * 60 * 60 * 1000;
        let beyond = NOW + 7 * 60 * 60 * 1000;
        assert!(schedule(RetryClass::Window { resets_at_ms: within }, 0, NOW, &settings, 1).is_some());
        assert!(schedule(RetryClass::Window { resets_at_ms: beyond }, 0, NOW, &settings, 1).is_none());
    }

    #[test]
    fn no_limit_schedules_a_week_out() {
        let a_week = NOW + 7 * 24 * 60 * 60 * 1000;
        assert!(
            schedule(RetryClass::Window { resets_at_ms: a_week }, 0, NOW, &unlimited(), 1).is_some()
        );
        assert!(
            schedule(
                RetryClass::Window { resets_at_ms: a_week },
                0,
                NOW,
                &RetrySettings::default(),
                1
            )
            .is_none(),
            "the 24h default must refuse a weekly window"
        );
    }

    #[test]
    fn the_default_ceiling_is_a_day() {
        assert_eq!(
            RetrySettings::default().max_automatic_wait,
            Some(Duration::from_secs(24 * 60 * 60))
        );
    }
}
