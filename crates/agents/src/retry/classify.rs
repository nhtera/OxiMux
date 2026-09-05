//! Deciding *why* a turn failed, from the typed signals the backend gave.
//!
//! The rule this module exists to enforce: a failure is retried only when
//! something machine-readable says waiting will help. Everything else — an
//! unknown limit kind, a spend cap, a plain error, a provider that said nothing
//! — falls through to [`RetryClass::Other`] and is surfaced to the user
//! untouched. Under-retrying costs a click; over-retrying spends money or
//! hammers an account that is already refusing requests.

use oximux_agent_core::thread::event::RateLimitInfo;

/// Why a turn failed, reduced to what the retry policy needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetryClass {
    /// The provider is transiently overloaded (HTTP 429/503/529). Retry soon,
    /// backing off — there is no reset time to aim at.
    Overload,
    /// A usage window is closed until the provider's reported reset.
    Window {
        /// Unix milliseconds.
        resets_at_ms: i64,
    },
    /// The account is billing past its plan, or hit a spend cap. Never retried:
    /// the next request costs real money the user did not approve.
    Spend,
    /// Anything else, including "the provider said nothing useful". Never
    /// retried.
    #[default]
    Other,
}

impl RetryClass {
    /// Whether this class is one the policy may schedule at all.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Overload | Self::Window { .. })
    }
}

/// HTTP statuses that mean "the provider is busy, come back shortly".
///
/// 529 is Anthropic's own overloaded status and is not in any HTTP registry,
/// which is exactly why it is listed explicitly rather than folded into a
/// `5xx` range check — a 500 is a bug report, not something to retry into.
const OVERLOAD_STATUSES: &[u16] = &[429, 503, 529];

/// Classify a failed turn.
///
/// `rate_limit` is the thread's latest `rate_limit_event` reading and
/// `turn_failure` the `(status, terminal_reason)` pair from the `TurnFailed`
/// that rides ahead of the errored `TurnEnded`. Both are typed wire signals;
/// neither is error prose.
///
/// The rate-limit reading is consulted first because it is the more specific
/// signal: a closed window surfaces as HTTP 429, and answering "retry in 30
/// seconds" to a window that reopens in six hours would burn all four attempts
/// inside two minutes and then report failure.
pub fn classify_failure(
    rate_limit: Option<&RateLimitInfo>,
    turn_failure: Option<(Option<u16>, Option<String>)>,
) -> RetryClass {
    if let Some(info) = rate_limit.filter(|i| i.is_rejected()) {
        return match (info.is_waitable_window(), info.resets_at_ms) {
            // A window we know how to wait out, and a reset to wait until.
            (true, Some(resets_at_ms)) => RetryClass::Window { resets_at_ms },
            // A known window whose reset the provider withheld. There is
            // nothing to schedule against, and guessing a duration is how a
            // retry storm starts.
            (true, None) => RetryClass::Other,
            // An overage: the plan allowance is gone and further requests bill.
            (false, _) if info.limit_type.as_deref().is_some_and(|t| t.contains("overage")) => {
                RetryClass::Spend
            }
            // A limit kind this build has never heard of.
            (false, _) => RetryClass::Other,
        };
    }
    match turn_failure {
        Some((Some(status), _)) if OVERLOAD_STATUSES.contains(&status) => RetryClass::Overload,
        _ => RetryClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(limit_type: &str, resets_at_ms: Option<i64>) -> RateLimitInfo {
        RateLimitInfo {
            status: "rejected".into(),
            resets_at_ms,
            limit_type: Some(limit_type.into()),
            utilization: Some(100.0),
        }
    }

    #[test]
    fn a_closed_window_with_a_reset_is_waitable() {
        assert_eq!(
            classify_failure(Some(&rejected("five_hour", Some(1_788_462_000_000))), None),
            RetryClass::Window { resets_at_ms: 1_788_462_000_000 }
        );
        assert_eq!(
            classify_failure(Some(&rejected("seven_day_opus", Some(9))), None),
            RetryClass::Window { resets_at_ms: 9 }
        );
    }

    /// The distinction that cannot be guessed from an error message: an overage
    /// is not a closed window, it is the account paying past its plan. Retrying
    /// it spends the user's money.
    #[test]
    fn an_overage_is_spend_and_is_never_retried() {
        for kind in ["overage", "seven_day_overage_included"] {
            let class = classify_failure(Some(&rejected(kind, Some(1))), None);
            assert_eq!(class, RetryClass::Spend, "{kind} must classify as spend");
            assert!(!class.is_retryable(), "{kind} must never be retried");
        }
    }

    #[test]
    fn an_unknown_limit_kind_is_not_retried() {
        let class = classify_failure(Some(&rejected("some_future_limit", Some(1))), None);
        assert_eq!(class, RetryClass::Other);
        assert!(!class.is_retryable());
    }

    /// A window with no reset time has nothing to schedule against. Inventing a
    /// duration here is how every thread on the account wakes at once.
    #[test]
    fn a_known_window_without_a_reset_is_not_retried() {
        assert_eq!(classify_failure(Some(&rejected("five_hour", None)), None), RetryClass::Other);
    }

    #[test]
    fn a_reading_that_is_not_rejected_does_not_classify_the_failure() {
        let allowed = RateLimitInfo {
            status: "allowed_warning".into(),
            resets_at_ms: Some(1),
            limit_type: Some("five_hour".into()),
            utilization: Some(92.0),
        };
        // Falls through to the status-based arm, which here says nothing.
        assert_eq!(classify_failure(Some(&allowed), None), RetryClass::Other);
        // ...and still reports overload when the status says so.
        assert_eq!(
            classify_failure(Some(&allowed), Some((Some(529), None))),
            RetryClass::Overload
        );
    }

    #[test]
    fn overload_statuses_are_the_listed_ones_only() {
        for status in [429, 503, 529] {
            assert_eq!(
                classify_failure(None, Some((Some(status), None))),
                RetryClass::Overload,
                "{status} is an overload"
            );
        }
        for status in [400, 401, 403, 404, 500, 502] {
            assert_eq!(
                classify_failure(None, Some((Some(status), None))),
                RetryClass::Other,
                "{status} must not be retried"
            );
        }
    }

    #[test]
    fn a_provider_that_said_nothing_is_never_retried() {
        assert_eq!(classify_failure(None, None), RetryClass::Other);
        assert_eq!(classify_failure(None, Some((None, Some("api_error".into())))), RetryClass::Other);
    }

    /// A closed window beats a bare 429 — the window carries a reset to aim at,
    /// and treating it as a transient overload would burn every attempt in two
    /// minutes and then report failure to the user.
    #[test]
    fn a_closed_window_wins_over_the_overload_status_it_arrives_with() {
        let class = classify_failure(
            Some(&rejected("five_hour", Some(1_788_462_000_000))),
            Some((Some(429), Some("api_error".into()))),
        );
        assert_eq!(class, RetryClass::Window { resets_at_ms: 1_788_462_000_000 });
    }
}
