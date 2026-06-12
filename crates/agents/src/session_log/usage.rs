//! Rate-limit-window estimation math (pure — no IO, no clock).
//!
//! The primary CLI enforces a rolling 5-hour activity block plus a weekly
//! ceiling, but exposes neither an API nor a local cache for the exact
//! numbers. What IS on disk is the per-message token usage in every
//! session log. This module turns hourly token buckets into window
//! estimates the status-bar meter can render.
//!
//! ESTIMATE-GRADE BY DESIGN (locked decision, validation session 1): the
//! meter ships with "~" labeling and a popover disclosure instead of
//! hiding. Both the token weighting and the per-tier budgets below are
//! calibration constants expected to be tuned against the CLI's own
//! numbers at the live drill — they live here, in one place, so tuning
//! is a one-line diff.

use std::collections::BTreeMap;

/// One rate-limit window estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Weighted tokens consumed inside the window.
    pub used_tokens: f64,
    /// Estimated window budget in the same weighted unit.
    pub budget_tokens: f64,
    /// When the window resets (unix ms). `None` for rolling windows (the
    /// weekly estimate) and for an idle 5-hour window (no active block).
    pub resets_at_ms: Option<i64>,
}

impl UsageWindow {
    /// Used fraction, clamped to [0, 1]. The meter renders this as `~NN%`.
    pub fn ratio(&self) -> f32 {
        if self.budget_tokens <= 0.0 {
            return 0.0;
        }
        (self.used_tokens / self.budget_tokens).clamp(0.0, 1.0) as f32
    }
}

/// Full probe output: both windows + the account tier they were scaled by.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub five_hour: UsageWindow,
    pub weekly: UsageWindow,
    /// Raw tier slug from the account config (e.g. `default_claude_max_5x`).
    pub tier: String,
}

/// Estimated weighted-token budgets for one account tier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TierBudget {
    pub block_tokens: f64,
    pub weekly_tokens: f64,
}

/// Hour-bucketed weighted tokens: key = bucket start (unix ms, floored to
/// the hour), value = weighted tokens in that hour.
pub type HourBuckets = BTreeMap<i64, f64>;

pub const HOUR_MS: i64 = 3_600_000;
/// The CLI's activity block length.
pub const BLOCK_MS: i64 = 5 * HOUR_MS;
/// Weekly window length (rolling approximation of the account week).
pub const WEEK_MS: i64 = 7 * 24 * HOUR_MS;

/// Map an account tier slug to its estimated budgets, `None` for unknown
/// tiers (the meter hides rather than guessing a denominator).
///
/// Calibration constants: block budgets follow the commonly-cited
/// community estimates for input+output tokens per 5-hour block; the
/// weekly budget is modeled as 10 blocks' worth. Tune at the live drill.
pub fn budget_for_tier(tier: &str) -> Option<TierBudget> {
    let block_tokens: f64 = if tier.contains("max_20x") {
        880_000.0
    } else if tier.contains("max_5x") {
        220_000.0
    } else if tier.contains("pro") {
        44_000.0
    } else {
        return None;
    };
    Some(TierBudget {
        block_tokens,
        weekly_tokens: block_tokens * 10.0,
    })
}

/// Weight one message's token usage. Output tokens count at face value
/// alongside input; cache reads/writes are excluded (they bill at deep
/// discounts and would swamp the estimate). Model multiplier mirrors the
/// price ratio between tiers of the primary CLI's model family.
pub fn weighted_tokens(input_tokens: f64, output_tokens: f64, model: &str) -> f64 {
    let mult = if model.contains("opus") {
        5.0
    } else if model.contains("haiku") {
        0.25
    } else {
        1.0
    };
    (input_tokens + output_tokens) * mult
}

/// Floor a timestamp to its hour bucket key.
pub fn bucket_key(ts_ms: i64) -> i64 {
    ts_ms - ts_ms.rem_euclid(HOUR_MS)
}

/// Locate the active 5-hour block and sum its usage.
///
/// Block model: the first activity opens a block anchored at its hour
/// bucket; activity past the block end opens a new block at ITS hour
/// bucket. The newest block is active when `now` is inside it; otherwise
/// the account is between blocks and the window reads 0 with no reset.
pub fn five_hour_window(buckets: &HourBuckets, now_ms: i64, budget_tokens: f64) -> UsageWindow {
    let mut block_start: Option<i64> = None;
    for (&h, &v) in buckets {
        if v <= 0.0 || h > now_ms {
            continue;
        }
        match block_start {
            None => block_start = Some(h),
            Some(bs) if h >= bs + BLOCK_MS => block_start = Some(h),
            _ => {}
        }
    }
    match block_start {
        Some(bs) if now_ms < bs + BLOCK_MS => {
            let used_tokens = buckets.range(bs..bs + BLOCK_MS).map(|(_, v)| v).sum();
            UsageWindow {
                used_tokens,
                budget_tokens,
                resets_at_ms: Some(bs + BLOCK_MS),
            }
        }
        _ => UsageWindow {
            used_tokens: 0.0,
            budget_tokens,
            resets_at_ms: None,
        },
    }
}

/// Rolling 7-day sum ending at `now`.
pub fn weekly_window(buckets: &HourBuckets, now_ms: i64, budget_tokens: f64) -> UsageWindow {
    let used_tokens = buckets
        .range((now_ms - WEEK_MS)..=now_ms)
        .map(|(_, v)| v)
        .sum();
    UsageWindow {
        used_tokens,
        budget_tokens,
        resets_at_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_781_222_400_000; // hour-aligned

    fn buckets(pairs: &[(i64, f64)]) -> HourBuckets {
        pairs.iter().copied().collect()
    }

    #[test]
    fn ratio_clamps_and_handles_zero_budget() {
        let w = UsageWindow {
            used_tokens: 50.0,
            budget_tokens: 0.0,
            resets_at_ms: None,
        };
        assert_eq!(w.ratio(), 0.0);
        let w = UsageWindow {
            used_tokens: 300.0,
            budget_tokens: 100.0,
            resets_at_ms: None,
        };
        assert_eq!(w.ratio(), 1.0);
    }

    #[test]
    fn budget_for_tier_known_tiers() {
        assert_eq!(
            budget_for_tier("default_claude_max_5x").unwrap().block_tokens,
            220_000.0
        );
        assert_eq!(
            budget_for_tier("default_claude_max_20x").unwrap().block_tokens,
            880_000.0
        );
        assert_eq!(budget_for_tier("claude_pro").unwrap().block_tokens, 44_000.0);
    }

    #[test]
    fn budget_for_tier_unknown_is_none() {
        assert!(budget_for_tier("enterprise_custom").is_none());
        assert!(budget_for_tier("").is_none());
    }

    #[test]
    fn weighted_tokens_applies_model_multiplier() {
        assert_eq!(weighted_tokens(100.0, 10.0, "claude-sonnet-4-6"), 110.0);
        assert_eq!(weighted_tokens(100.0, 10.0, "claude-opus-4-8"), 550.0);
        assert_eq!(weighted_tokens(100.0, 10.0, "claude-haiku-4-5"), 27.5);
    }

    #[test]
    fn bucket_key_floors_to_hour() {
        assert_eq!(bucket_key(T0 + 59 * 60_000), T0);
        assert_eq!(bucket_key(T0 + HOUR_MS), T0 + HOUR_MS);
    }

    #[test]
    fn five_hour_active_block_sums_and_sets_reset() {
        // Activity at T0 and T0+2h; now = T0+3h → block [T0, T0+5h).
        let b = buckets(&[(T0, 100.0), (T0 + 2 * HOUR_MS, 50.0)]);
        let w = five_hour_window(&b, T0 + 3 * HOUR_MS, 1000.0);
        assert_eq!(w.used_tokens, 150.0);
        assert_eq!(w.resets_at_ms, Some(T0 + BLOCK_MS));
    }

    #[test]
    fn five_hour_expired_block_reads_zero() {
        let b = buckets(&[(T0, 100.0)]);
        let w = five_hour_window(&b, T0 + 6 * HOUR_MS, 1000.0);
        assert_eq!(w.used_tokens, 0.0);
        assert_eq!(w.resets_at_ms, None);
    }

    #[test]
    fn five_hour_second_block_excludes_first() {
        // First block at T0; new activity at T0+7h opens a second block.
        let b = buckets(&[(T0, 100.0), (T0 + 7 * HOUR_MS, 40.0)]);
        let w = five_hour_window(&b, T0 + 8 * HOUR_MS, 1000.0);
        assert_eq!(w.used_tokens, 40.0);
        assert_eq!(w.resets_at_ms, Some(T0 + 7 * HOUR_MS + BLOCK_MS));
    }

    #[test]
    fn five_hour_ignores_future_buckets() {
        // A clock-skewed entry an hour in the future must not anchor or
        // count toward the active block.
        let b = buckets(&[(T0, 100.0), (T0 + HOUR_MS, 30.0)]);
        let w = five_hour_window(&b, T0, 1000.0);
        assert_eq!(w.used_tokens, 100.0 + 30.0); // both inside [T0, T0+5h)
        let b2 = buckets(&[(T0 + HOUR_MS, 30.0)]);
        let w2 = five_hour_window(&b2, T0, 1000.0);
        assert_eq!(w2.used_tokens, 0.0, "future-only activity is no block");
    }

    #[test]
    fn five_hour_empty_buckets_reads_zero() {
        let w = five_hour_window(&HourBuckets::new(), T0, 1000.0);
        assert_eq!(w.used_tokens, 0.0);
        assert_eq!(w.resets_at_ms, None);
    }

    #[test]
    fn weekly_sums_only_last_seven_days() {
        let b = buckets(&[
            (T0 - WEEK_MS - HOUR_MS, 999.0), // 8 days ago — out
            (T0 - 3 * 24 * HOUR_MS, 200.0),  // 3 days ago — in
            (T0 - HOUR_MS, 50.0),            // 1 hour ago — in
        ]);
        let w = weekly_window(&b, T0, 10_000.0);
        assert_eq!(w.used_tokens, 250.0);
        assert_eq!(w.resets_at_ms, None);
    }
}
