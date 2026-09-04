//! `UsageProbe` — blocking sampler producing one [`ProviderUsage`] per
//! configured account.
//!
//! Each provider publishes its usage somewhere different, so each is a
//! [`UsageSource`]: the primary CLI's account usage API (`usage_oauth`), and
//! the rate-limit windows a second CLI journals into its session rollouts
//! (`usage_codex`). Adding a third is an impl, not a rewrite.
//!
//! The sampler owns what is common to all of them and nothing else: a failure
//! backoff so a declined Keychain prompt does not re-ask every tick, the
//! reason from the last failure so a tick inside that backoff still names a
//! cause, and a last-known-good reading for sources that can use one. It never
//! falls back to a local-log estimate — a guessed percentage presented as real
//! is worse than an honest "unavailable".
//!
//! A provider that has left no trace on the machine yields no row at all. That
//! is not the same as a row reporting a failure: "you do not use this CLI" and
//! "this CLI is broken" must not look alike.
//!
//! Blocking shellout (Keychain + curl) and file reads — callers MUST run
//! `sample()` on a background executor.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use super::now_unix_ms;
use super::usage::{
    FIVE_HOUR_MINUTES, ProviderUsage, UsageProvider, UsageSnapshot, UsageState, UsageWindow,
    WEEK_MINUTES,
};
use super::usage_oauth::{FetchError, OauthUsage};
use super::usage_codex;

/// Why one attempt did not produce a reading, and how long to wait before the
/// next one. The provider picks both — a Keychain prompt and a local file read
/// deserve very different patience.
pub struct SourceFailure {
    /// User-facing cause, shown in the popover.
    pub reason: String,
    /// Do not attempt again for this long.
    pub backoff_ms: i64,
}

/// One account's usage, wherever that provider happens to keep it.
pub trait UsageSource: Send + Sync {
    /// Which account this reads.
    fn provider(&self) -> UsageProvider;

    /// Whether this provider has left any trace on this machine. False means
    /// no meter row at all.
    fn is_configured(&self) -> bool;

    /// One attempt at a reading.
    fn fetch(&self, now_ms: i64) -> Result<UsageSnapshot, SourceFailure>;

    /// How long a good reading may stand in for a later failed fetch.
    ///
    /// Meaningful only for a source fetched live, where an outage is transient
    /// and the previous answer is still roughly true. A source that reads an
    /// already-dated record returns zero: it has nothing to gain, because the
    /// record it would re-serve is the same one it just failed to read, and
    /// its validity is bounded by its own reset times rather than by a clock
    /// started when we happened to read it.
    fn last_good_max_age_ms(&self) -> i64;
}

/// One sampled state source. Object-safe so the status-bar code can hold
/// `Arc<dyn UsageProbe>` and tests can substitute a fixture probe.
pub trait UsageProbe: Send + Sync {
    /// Take one sample: a row per configured provider, in a stable order.
    /// Empty only when no provider is set up at all.
    fn sample(&self) -> Vec<ProviderUsage>;
}

/// Backoff after a failed attempt. A missing token is a standing condition —
/// back off long so we don't re-prompt the Keychain every tick. A rejected
/// (expired) token or an unreachable endpoint is transient — the CLI
/// refreshes the token on its next authenticated call, so retry soon and let
/// `last_good` cover the brief gap.
const BACKOFF_NO_TOKEN_MS: i64 = 15 * 60_000;
const BACKOFF_TRANSIENT_MS: i64 = 60_000;

/// Backoff after a rollout read produced nothing. The answer only changes when
/// the user takes another turn, so re-walking the session tree every tick
/// would re-read the same megabyte to reach the same conclusion.
const BACKOFF_NO_ROLLOUT_MS: i64 = 5 * 60_000;

/// How long a cached reading may stand in for a live one. Past this the
/// percentages are too stale to present as current.
const LAST_GOOD_MAX_AGE_MS: i64 = 30 * 60_000;

/// The primary CLI's account usage API, with the plan tier read from its
/// config file for display.
struct OauthSource {
    home: PathBuf,
    /// Off in tests — the real fetch shells out to the Keychain and network.
    enabled: bool,
}

impl UsageSource for OauthSource {
    fn provider(&self) -> UsageProvider {
        UsageProvider::ClaudeCode
    }

    /// Always a row. This is the primary CLI: when it is not signed in, "Not
    /// signed in" is the useful thing to say, and it is what the meter said
    /// before there was more than one provider.
    fn is_configured(&self) -> bool {
        true
    }

    fn last_good_max_age_ms(&self) -> i64 {
        LAST_GOOD_MAX_AGE_MS
    }

    fn fetch(&self, _now_ms: i64) -> Result<UsageSnapshot, SourceFailure> {
        if !self.enabled {
            return Err(SourceFailure {
                reason: default_reason(),
                backoff_ms: BACKOFF_TRANSIENT_MS,
            });
        }
        match super::usage_oauth::fetch(&self.home) {
            Ok(usage) => {
                let tier = read_account_tier(&self.home.join(".claude.json")).unwrap_or_default();
                Ok(snapshot_from_usage(usage, tier))
            }
            Err(err) => {
                let (backoff_ms, reason) = describe_failure(&err);
                Err(SourceFailure { reason, backoff_ms })
            }
        }
    }
}

/// The second CLI's rate-limit windows, read back out of its session rollouts.
struct RolloutSource {
    home: PathBuf,
}

impl UsageSource for RolloutSource {
    fn provider(&self) -> UsageProvider {
        UsageProvider::Codex
    }

    fn is_configured(&self) -> bool {
        usage_codex::is_configured(&self.home)
    }

    /// Zero: what it reads already carries the moment it was captured, and
    /// each window's own reset time bounds how long it stays true.
    fn last_good_max_age_ms(&self) -> i64 {
        0
    }

    fn fetch(&self, now_ms: i64) -> Result<UsageSnapshot, SourceFailure> {
        usage_codex::fetch(&self.home, now_ms).map_err(|reason| SourceFailure {
            reason,
            backoff_ms: BACKOFF_NO_ROLLOUT_MS,
        })
    }
}

/// A source plus the sampler's memory of how it has been behaving.
struct SourceSlot {
    source: Box<dyn UsageSource>,
    /// Don't re-attempt before this instant after a failure.
    backoff_until_ms: Mutex<i64>,
    /// Reason from the most recent failure, so ticks inside the backoff window
    /// still render the cause instead of a bare "unavailable".
    last_error: Mutex<Option<String>>,
    /// Most recent good reading + the unix-ms instant it was taken. Only kept
    /// for sources whose `last_good_max_age_ms` is non-zero.
    last_good: Mutex<Option<(UsageSnapshot, i64)>>,
}

impl SourceSlot {
    fn new(source: Box<dyn UsageSource>) -> Self {
        Self {
            source,
            backoff_until_ms: Mutex::new(0),
            last_error: Mutex::new(None),
            last_good: Mutex::new(None),
        }
    }

    /// The reason from the last failure, or a generic fallback before any
    /// attempt has run.
    fn remembered_reason(&self) -> String {
        lock(&self.last_error).clone().unwrap_or_else(default_reason)
    }

    /// This slot's state for one tick.
    fn sample(&self, now_ms: i64) -> UsageState {
        // 1. A fresh attempt, unless we are still backing off from the last
        //    failure — in which case the remembered reason stands.
        let attempt = if now_ms < *lock(&self.backoff_until_ms) {
            Err(self.remembered_reason())
        } else {
            match self.source.fetch(now_ms) {
                Ok(snapshot) => Ok(snapshot),
                Err(failure) => {
                    *lock(&self.backoff_until_ms) = now_ms + failure.backoff_ms;
                    Err(failure.reason)
                }
            }
        };

        match attempt {
            Ok(snapshot) => {
                if self.source.last_good_max_age_ms() > 0 {
                    *lock(&self.last_good) = Some((snapshot.clone(), now_ms));
                }
                *lock(&self.last_error) = None;
                return UsageState::Available(snapshot);
            }
            Err(reason) => *lock(&self.last_error) = Some(reason),
        }

        // 2. A recent good reading still beats nothing: slightly-stale
        //    percentages, but accurate reset countdowns. Marked captured so the
        //    UI discloses "updated N ago" rather than presenting it as live.
        let max_age = self.source.last_good_max_age_ms();
        if max_age > 0
            && let Some((mut good, captured_at)) = lock(&self.last_good).clone()
            && now_ms - captured_at <= max_age
        {
            good.captured_at_ms = Some(captured_at);
            return UsageState::Available(good);
        }

        // 3. Nothing usable — surface the cause, never a guessed number.
        UsageState::Unavailable {
            reason: self.remembered_reason(),
        }
    }
}

/// The shipped sampler: every provider OxiMux knows how to read, in a stable
/// display order.
pub struct SessionLogUsageProbe {
    slots: Vec<SourceSlot>,
}

impl SessionLogUsageProbe {
    pub fn new(home: PathBuf) -> Self {
        Self::from_sources(vec![
            Box::new(OauthSource {
                home: home.clone(),
                enabled: true,
            }),
            Box::new(RolloutSource { home }),
        ])
    }

    fn from_sources(sources: Vec<Box<dyn UsageSource>>) -> Self {
        Self {
            slots: sources.into_iter().map(SourceSlot::new).collect(),
        }
    }
}

impl UsageProbe for SessionLogUsageProbe {
    fn sample(&self) -> Vec<ProviderUsage> {
        let now = now_unix_ms();
        self.slots
            .iter()
            // Configuration is re-checked every tick, not at construction: a
            // CLI installed while the app is running should get its row without
            // a restart.
            .filter(|slot| slot.source.is_configured())
            .map(|slot| ProviderUsage {
                provider: slot.source.provider(),
                state: slot.sample(now),
            })
            .collect()
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
/// Shortest span first, which is the order the meter renders them.
fn snapshot_from_usage(usage: OauthUsage, tier: String) -> UsageSnapshot {
    UsageSnapshot {
        windows: vec![
            UsageWindow {
                window_minutes: FIVE_HOUR_MINUTES,
                utilization: usage.five_hour.utilization,
                resets_at_ms: usage.five_hour.resets_at_ms,
            },
            UsageWindow {
                window_minutes: WEEK_MINUTES,
                utilization: usage.seven_day.utilization,
                resets_at_ms: usage.seven_day.resets_at_ms,
            },
        ],
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn exact(util_five: f64, util_weekly: f64) -> UsageSnapshot {
        UsageSnapshot {
            windows: vec![
                UsageWindow {
                    window_minutes: FIVE_HOUR_MINUTES,
                    utilization: util_five,
                    resets_at_ms: Some(99_000_000),
                },
                UsageWindow {
                    window_minutes: WEEK_MINUTES,
                    utilization: util_weekly,
                    resets_at_ms: Some(600_000_000),
                },
            ],
            tier: "default_claude_max_5x".to_string(),
            captured_at_ms: None,
        }
    }

    /// A source driven entirely by the test: no files, no network, no clock.
    struct FakeSource {
        provider: UsageProvider,
        configured: bool,
        max_age_ms: i64,
        /// Answers, consumed in order; the last one repeats once exhausted.
        answers: Mutex<Vec<Result<UsageSnapshot, String>>>,
        calls: AtomicUsize,
    }

    impl FakeSource {
        fn new(provider: UsageProvider, answers: Vec<Result<UsageSnapshot, String>>) -> Self {
            Self {
                provider,
                configured: true,
                max_age_ms: LAST_GOOD_MAX_AGE_MS,
                answers: Mutex::new(answers),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl UsageSource for FakeSource {
        fn provider(&self) -> UsageProvider {
            self.provider
        }
        fn is_configured(&self) -> bool {
            self.configured
        }
        fn last_good_max_age_ms(&self) -> i64 {
            self.max_age_ms
        }
        fn fetch(&self, _now_ms: i64) -> Result<UsageSnapshot, SourceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut answers = lock(&self.answers);
            let answer = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers.first().cloned().unwrap_or(Err("empty".to_string()))
            };
            answer.map_err(|reason| SourceFailure {
                reason,
                backoff_ms: 0,
            })
        }
    }

    #[test]
    fn a_failing_source_reports_its_own_reason() {
        let slot = SourceSlot::new(Box::new(FakeSource::new(
            UsageProvider::ClaudeCode,
            vec![Err("Not signed in".to_string())],
        )));
        match slot.sample(0) {
            UsageState::Unavailable { reason } => assert_eq!(reason, "Not signed in"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn fresh_last_good_serves_a_captured_reading() {
        // A recent good reading must keep showing (the live path is down this
        // tick) rather than flipping straight to "unavailable".
        let snap = exact(26.0, 27.0);
        let slot = SourceSlot::new(Box::new(FakeSource::new(
            UsageProvider::ClaudeCode,
            vec![Ok(snap.clone()), Err("Temporarily unavailable".to_string())],
        )));
        assert!(matches!(slot.sample(0), UsageState::Available(_)));
        match slot.sample(60_000) {
            UsageState::Available(got) => {
                assert_eq!(got.windows, snap.windows);
                assert_eq!(
                    got.captured_at_ms,
                    Some(0),
                    "a served cache must carry its capture time so the UI discloses staleness"
                );
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn stale_last_good_falls_through_to_unavailable() {
        let slot = SourceSlot::new(Box::new(FakeSource::new(
            UsageProvider::ClaudeCode,
            vec![Ok(exact(26.0, 27.0)), Err("Temporarily unavailable".to_string())],
        )));
        assert!(matches!(slot.sample(0), UsageState::Available(_)));
        let past_the_window = LAST_GOOD_MAX_AGE_MS + 1;
        assert!(matches!(
            slot.sample(past_the_window),
            UsageState::Unavailable { .. }
        ));
    }

    #[test]
    fn a_source_that_keeps_no_cache_does_not_serve_one() {
        // The rollout source's contract: what it reads is already dated, so a
        // failure must surface as a failure rather than resurrect an old read.
        let mut fake = FakeSource::new(
            UsageProvider::Codex,
            vec![Ok(exact(6.0, 23.0)), Err("Every window has reset".to_string())],
        );
        fake.max_age_ms = 0;
        let slot = SourceSlot::new(Box::new(fake));
        assert!(matches!(slot.sample(0), UsageState::Available(_)));
        match slot.sample(1_000) {
            UsageState::Unavailable { reason } => assert_eq!(reason, "Every window has reset"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_backoff_suppresses_the_next_attempt_but_keeps_the_reason() {
        let source = FakeSource {
            provider: UsageProvider::ClaudeCode,
            configured: true,
            max_age_ms: 0,
            answers: Mutex::new(vec![Err("Not signed in".to_string())]),
            calls: AtomicUsize::new(0),
        };
        let slot = SourceSlot {
            source: Box::new(source),
            backoff_until_ms: Mutex::new(0),
            last_error: Mutex::new(None),
            last_good: Mutex::new(None),
        };
        // Force a long backoff by failing once with one, then sampling inside it.
        *lock(&slot.backoff_until_ms) = 0;
        assert!(matches!(slot.sample(0), UsageState::Unavailable { .. }));
        *lock(&slot.backoff_until_ms) = 10_000;
        match slot.sample(5_000) {
            UsageState::Unavailable { reason } => {
                assert_eq!(reason, "Not signed in", "the cause must survive the backoff")
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn an_unconfigured_provider_gets_no_row() {
        let mut absent = FakeSource::new(UsageProvider::Codex, vec![Ok(exact(1.0, 2.0))]);
        absent.configured = false;
        let probe = SessionLogUsageProbe::from_sources(vec![
            Box::new(FakeSource::new(
                UsageProvider::ClaudeCode,
                vec![Ok(exact(12.0, 4.0))],
            )),
            Box::new(absent),
        ]);
        let rows = probe.sample();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, UsageProvider::ClaudeCode);
    }

    #[test]
    fn one_provider_failing_does_not_blank_the_other() {
        let probe = SessionLogUsageProbe::from_sources(vec![
            Box::new(FakeSource::new(
                UsageProvider::ClaudeCode,
                vec![Ok(exact(12.0, 4.0))],
            )),
            Box::new(FakeSource::new(
                UsageProvider::Codex,
                vec![Err("Every window has reset".to_string())],
            )),
        ]);
        let rows = probe.sample();
        assert_eq!(rows.len(), 2, "both configured providers keep their row");
        assert!(matches!(rows[0].state, UsageState::Available(_)));
        match &rows[1].state {
            UsageState::Unavailable { reason } => assert_eq!(reason, "Every window has reset"),
            other => panic!("expected the second provider to name its own cause, got {other:?}"),
        }
    }

    #[test]
    fn the_primary_reading_keeps_its_exact_two_windows() {
        // The refactor must not move a number: same utilizations, same resets,
        // same order, from the same API response shape as before.
        let usage = OauthUsage {
            five_hour: super::super::usage_oauth::OauthWindow {
                utilization: 12.0,
                resets_at_ms: Some(10_000_000),
            },
            seven_day: super::super::usage_oauth::OauthWindow {
                utilization: 4.0,
                resets_at_ms: Some(600_000_000),
            },
        };
        let snap = snapshot_from_usage(usage, "default_claude_max_5x".to_string());
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.windows[0].window_minutes, FIVE_HOUR_MINUTES);
        assert_eq!(snap.windows[0].utilization, 12.0);
        assert_eq!(snap.windows[0].resets_at_ms, Some(10_000_000));
        assert_eq!(snap.windows[1].window_minutes, WEEK_MINUTES);
        assert_eq!(snap.windows[1].utilization, 4.0);
        assert_eq!(snap.windows[1].resets_at_ms, Some(600_000_000));
        assert_eq!(snap.tier, "default_claude_max_5x");
        assert_eq!(snap.captured_at_ms, None);
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
