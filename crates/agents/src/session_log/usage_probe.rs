//! `UsageProbe` — blocking sampler producing a [`UsageState`].
//!
//! The only source is the account usage API (`usage_oauth`): exact,
//! account-wide window percentages — the same numbers the CLI's own usage
//! panel shows. When the API can't be reached or authenticated, a recent
//! exact reading stands in for up to [`LAST_GOOD_MAX_AGE_MS`]; past that the
//! probe reports [`UsageState::Unavailable`] with the failure reason. It
//! never falls back to a local-log estimate — a guessed percentage presented
//! as real is worse than an honest "unavailable".
//!
//! Blocking shellout (Keychain + curl) — callers MUST run `sample()` on a
//! background executor.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use super::now_unix_ms;
use super::usage::{UsageSnapshot, UsageState, UsageWindow};
use super::usage_oauth::{FetchError, OauthUsage};

/// One sampled state source. Object-safe so the status-bar code can hold
/// `Arc<dyn UsageProbe>` and tests can substitute a fixture probe.
pub trait UsageProbe: Send + Sync {
    /// Take one sample. Always returns a state: `Available` with a reading,
    /// or `Unavailable` with the reason the account usage API didn't answer.
    fn sample(&self) -> UsageState;
}

/// Probe over the primary CLI's account usage API, with the account tier read
/// from `<home>/.claude.json` for display.
pub struct SessionLogUsageProbe {
    home: PathBuf,
    /// Off in tests — the real fetch shells out to the Keychain and network.
    oauth_enabled: bool,
    /// Don't re-attempt the API before this instant after a failure: a
    /// Keychain decline would otherwise re-prompt on every tick.
    backoff_until_ms: Mutex<i64>,
    /// Reason from the most recent failed fetch, so ticks within the backoff
    /// window still render the cause instead of a bare "unavailable".
    last_error: Mutex<Option<String>>,
    /// Most recent exact reading + the unix-ms instant it was taken. Covers a
    /// brief outage (expired token mid-refresh, momentary offline) while
    /// younger than [`LAST_GOOD_MAX_AGE_MS`]: the percentages are slightly
    /// stale but the reset timestamps stay absolute (still count down right).
    last_good: Mutex<Option<(UsageSnapshot, i64)>>,
}

/// Backoff after a failed attempt. A missing token is a standing condition —
/// back off long so we don't re-prompt the Keychain every tick. A rejected
/// (expired) token or an unreachable endpoint is transient — the CLI
/// refreshes the token on its next authenticated call, so retry soon and let
/// `last_good` cover the brief gap.
const BACKOFF_NO_TOKEN_MS: i64 = 15 * 60_000;
const BACKOFF_TRANSIENT_MS: i64 = 60_000;

/// How long a cached reading may stand in for a live one. Past this the
/// percentages are too stale to present as current (mirrors the reference
/// cockpit's 30-minute stale-data window).
const LAST_GOOD_MAX_AGE_MS: i64 = 30 * 60_000;

impl SessionLogUsageProbe {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            oauth_enabled: true,
            backoff_until_ms: Mutex::new(0),
            last_error: Mutex::new(None),
            last_good: Mutex::new(None),
        }
    }

    /// Try the account usage API, honoring the failure backoff. `Ok` is a
    /// fresh reading; `Err` is the user-facing reason it was unavailable.
    fn fetch_oauth(&self, now_ms: i64) -> Result<UsageSnapshot, String> {
        if !self.oauth_enabled || now_ms < *lock(&self.backoff_until_ms) {
            return Err(self.remembered_reason());
        }
        match super::usage_oauth::fetch(&self.home) {
            Ok(usage) => {
                let tier = read_account_tier(&self.home.join(".claude.json")).unwrap_or_default();
                Ok(snapshot_from_usage(usage, tier))
            }
            Err(err) => {
                let (backoff, reason) = describe_failure(&err);
                *lock(&self.backoff_until_ms) = now_ms + backoff;
                Err(reason)
            }
        }
    }

    /// The reason from the last failed fetch, or a generic fallback before any
    /// fetch has run (the test-only offline path, or the first backoff tick).
    fn remembered_reason(&self) -> String {
        lock(&self.last_error).clone().unwrap_or_else(default_reason)
    }
}

impl UsageProbe for SessionLogUsageProbe {
    fn sample(&self) -> UsageState {
        let now = now_unix_ms();

        // 1. Live exact reading — the source of truth.
        match self.fetch_oauth(now) {
            Ok(snapshot) => {
                *lock(&self.last_good) = Some((snapshot.clone(), now));
                *lock(&self.last_error) = None;
                return UsageState::Available(snapshot);
            }
            Err(reason) => *lock(&self.last_error) = Some(reason),
        }

        // 2. A recent exact reading still beats nothing: slightly-stale
        //    percentages, but accurate reset countdowns. Marked cached so the
        //    UI discloses "updated N ago" rather than presenting it as live.
        if let Some((mut good, cached_at)) = lock(&self.last_good).clone()
            && now - cached_at <= LAST_GOOD_MAX_AGE_MS
        {
            good.captured_at_ms = Some(cached_at);
            return UsageState::Available(good);
        }

        // 3. Nothing usable — surface the cause, never a guessed number.
        UsageState::Unavailable {
            reason: self.remembered_reason(),
        }
    }
}

/// Lock a mutex, recovering the inner value if a prior holder panicked.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Map a fetch failure to its `(backoff, user-facing reason)`.
fn describe_failure(err: &FetchError) -> (i64, String) {
    match err {
        FetchError::NoToken => (BACKOFF_NO_TOKEN_MS, "Not signed in".to_string()),
        FetchError::Unauthorized(msg) => (BACKOFF_TRANSIENT_MS, msg.clone()),
        FetchError::Unreachable => (BACKOFF_TRANSIENT_MS, "Temporarily unavailable".to_string()),
    }
}

fn default_reason() -> String {
    "Usage data is unavailable".to_string()
}

/// Build a display snapshot from the API's two windows + the account tier.
fn snapshot_from_usage(usage: OauthUsage, tier: String) -> UsageSnapshot {
    UsageSnapshot {
        five_hour: UsageWindow {
            utilization: usage.five_hour.utilization,
            resets_at_ms: usage.five_hour.resets_at_ms,
        },
        weekly: UsageWindow {
            utilization: usage.seven_day.utilization,
            resets_at_ms: usage.seven_day.resets_at_ms,
        },
        tier,
        captured_at_ms: None,
    }
}

/// The account's rate-limit tier slug from the CLI config, if present —
/// shown in the popover (e.g. `default_claude_max_5x`).
fn read_account_tier(config_path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(config_path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.pointer("/oauthAccount/organizationRateLimitTier")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
impl SessionLogUsageProbe {
    /// Offline probe: skips the real network/Keychain fetch so tests drive
    /// the last-known-good and unavailable paths deterministically.
    fn offline(home: PathBuf) -> Self {
        Self {
            oauth_enabled: false,
            ..Self::new(home)
        }
    }

    fn seed_last_good(&self, snapshot: UsageSnapshot, cached_at_ms: i64) {
        *lock(&self.last_good) = Some((snapshot, cached_at_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(util_five: f64, util_weekly: f64) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: UsageWindow {
                utilization: util_five,
                resets_at_ms: Some(99_000_000),
            },
            weekly: UsageWindow {
                utilization: util_weekly,
                resets_at_ms: Some(600_000_000),
            },
            tier: "default_claude_max_5x".to_string(),
            captured_at_ms: None,
        }
    }

    #[test]
    fn unavailable_without_token_or_cache() {
        let home = tempfile::tempdir().unwrap();
        let probe = SessionLogUsageProbe::offline(home.path().to_path_buf());
        match probe.sample() {
            UsageState::Unavailable { reason } => assert!(!reason.is_empty()),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn fresh_last_good_serves_cached_reading() {
        // A recent exact reading must keep showing (the live path is down this
        // tick) rather than flipping straight to "unavailable".
        let home = tempfile::tempdir().unwrap();
        let probe = SessionLogUsageProbe::offline(home.path().to_path_buf());
        let snap = exact(26.0, 27.0);
        probe.seed_last_good(snap.clone(), now_unix_ms());
        match probe.sample() {
            UsageState::Available(got) => {
                assert_eq!(got.five_hour, snap.five_hour);
                assert_eq!(got.weekly, snap.weekly);
                assert!(
                    got.captured_at_ms.is_some(),
                    "cached reading must carry a capture time so the UI discloses staleness"
                );
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn stale_last_good_falls_through_to_unavailable() {
        // A cached reading older than the stale window must not stand in for a
        // live one — with the estimate gone, that means "unavailable".
        let home = tempfile::tempdir().unwrap();
        let probe = SessionLogUsageProbe::offline(home.path().to_path_buf());
        probe.seed_last_good(exact(26.0, 27.0), now_unix_ms() - 31 * 60_000);
        assert!(matches!(probe.sample(), UsageState::Unavailable { .. }));
    }

    #[test]
    fn read_account_tier_reads_slug() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"oauthAccount":{"organizationRateLimitTier":"default_claude_max_5x"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_account_tier(&cfg).as_deref(),
            Some("default_claude_max_5x")
        );
        assert!(read_account_tier(&home.path().join("missing.json")).is_none());
    }

    #[test]
    fn describe_failure_names_each_cause() {
        assert_eq!(describe_failure(&FetchError::NoToken).1, "Not signed in");
        assert_eq!(
            describe_failure(&FetchError::Unauthorized(
                "Invalid authentication credentials".to_string()
            ))
            .1,
            "Invalid authentication credentials"
        );
        assert_eq!(
            describe_failure(&FetchError::Unreachable).1,
            "Temporarily unavailable"
        );
    }
}
