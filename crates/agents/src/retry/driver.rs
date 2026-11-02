//! What to do with a turn that failed on a provider limit, given how many
//! automatic attempts it has already had.
//!
//! [`super::schedule`] answers *when*; this adds the one distinction a host
//! needs to report the outcome honestly: a turn that will never be retried
//! because the failure is not the retryable kind is an ordinary failure, but
//! one that ran out of attempts says so ("rate limit retries exhausted").
//! Pure, like the rest of the module; the attempt count comes from the
//! durable [`super::store::RetryStore`], so it survives restarts.

use super::{MAX_ATTEMPTS, RetryClass, RetrySettings, schedule};

/// The decision for a failed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Retry at `wake_at_ms`; it will be attempt `attempt` (zero-based).
    Arm { wake_at_ms: i64, attempt: u32 },
    /// Retryable, but every automatic attempt is spent (or the wait would
    /// exceed the user's limit).
    Exhausted,
    /// Not a failure waiting can fix.
    Never,
}

/// Decide for a turn that failed with `class` after `attempts` automatic
/// attempts.
pub fn decide(class: RetryClass, attempts: u32, now_ms: i64, settings: &RetrySettings, seed: u64) -> Next {
    if !class.is_retryable() {
        return Next::Never;
    }
    if attempts >= MAX_ATTEMPTS {
        return Next::Exhausted;
    }
    match schedule(class, attempts, now_ms, settings, seed) {
        Some(wake_at_ms) => Next::Arm { wake_at_ms, attempt: attempts },
        None => Next::Exhausted,
    }
}

/// The class as the retry store records it.
pub fn class_text(class: RetryClass) -> &'static str {
    match class {
        RetryClass::Window { .. } => "window",
        RetryClass::Overload => "overload",
        RetryClass::Spend => "spend",
        RetryClass::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_788_000_000_000;

    #[test]
    fn a_retryable_turn_is_armed_until_its_attempts_run_out() {
        let s = RetrySettings::default();
        assert!(matches!(decide(RetryClass::Overload, 0, NOW, &s, 1), Next::Arm { attempt: 0, .. }));
        assert!(matches!(decide(RetryClass::Overload, MAX_ATTEMPTS - 1, NOW, &s, 1), Next::Arm { .. }));
        assert_eq!(decide(RetryClass::Overload, MAX_ATTEMPTS, NOW, &s, 1), Next::Exhausted);
    }

    #[test]
    fn a_failure_waiting_cannot_fix_is_never_retried() {
        assert_eq!(decide(RetryClass::Spend, 0, NOW, &RetrySettings::default(), 1), Next::Never);
        assert_eq!(decide(RetryClass::Other, 0, NOW, &RetrySettings::default(), 1), Next::Never);
    }

    #[test]
    fn a_wait_past_the_users_limit_is_exhausted_not_never() {
        let s = RetrySettings { max_automatic_wait: Some(std::time::Duration::from_secs(60)) };
        let far = RetryClass::Window { resets_at_ms: NOW + 6 * 3_600_000 };
        assert_eq!(decide(far, 0, NOW, &s, 1), Next::Exhausted);
    }
}
