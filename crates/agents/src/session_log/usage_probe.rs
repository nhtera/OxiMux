//! `UsageProbe` — blocking sampler producing a [`UsageSnapshot`].
//!
//! Strategy order: the account usage API first (`usage_oauth` — exact
//! account-wide percentages, the same numbers the CLI's own usage panel
//! shows), then the local session-log token tally as the estimate-grade
//! fallback when the API is unreachable (no credentials, offline,
//! Keychain declined).
//!
//! Blocking file IO throughout — callers MUST run `sample()` on a
//! background executor. A per-file `(mtime, len)` cache keeps steady-state
//! ticks cheap: only logs that changed since the last sample re-parse.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use super::usage::{
    HourBuckets, UsageSnapshot, UsageSource, UsageWindow, WEEK_MS, bucket_key, budget_for_tier,
    five_hour_window, weekly_window, weighted_tokens,
};
use super::{now_unix_ms, parse_timestamp_ms};

/// Logs untouched for longer than this are ignored (and their cache
/// entries evicted). One day past the weekly window so a log written
/// right at the window edge still contributes.
const SCAN_HORIZON_MS: i64 = WEEK_MS + 24 * 3_600_000;

/// One sampled snapshot source. Object-safe so the status-bar code can
/// hold `Arc<dyn UsageProbe>` and tests can substitute a fixture probe.
pub trait UsageProbe: Send + Sync {
    /// Take one sample. `None` = no displayable data (no account config,
    /// unknown tier, no logs) — the meter hides entirely.
    fn sample(&self) -> Option<UsageSnapshot>;
}

/// Probe over the primary CLI's on-disk state, rooted at the user's home
/// directory: `<home>/.claude.json` (account tier) and
/// `<home>/.claude/projects/**/*.jsonl` (session logs).
pub struct SessionLogUsageProbe {
    home: PathBuf,
    cache: Mutex<HashMap<PathBuf, FileTally>>,
    /// The account usage API is tried before the tally (exact numbers).
    /// Off in tests — the fetch shells out to the Keychain and network.
    oauth_enabled: bool,
    /// Don't re-attempt the API before this instant after a failure: a
    /// Keychain decline would otherwise re-prompt on every 60 s tick.
    oauth_backoff_until_ms: Mutex<i64>,
}

struct FileTally {
    mtime_ms: i64,
    len: u64,
    buckets: HourBuckets,
}

/// After a failed API attempt, stay on the estimate path this long.
const OAUTH_BACKOFF_MS: i64 = 15 * 60_000;

impl SessionLogUsageProbe {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            cache: Mutex::new(HashMap::new()),
            oauth_enabled: true,
            oauth_backoff_until_ms: Mutex::new(0),
        }
    }

    /// Estimate-only probe: skips the account usage API entirely.
    pub fn new_estimate_only(home: PathBuf) -> Self {
        Self {
            oauth_enabled: false,
            ..Self::new(home)
        }
    }

    /// Try the account usage API, honoring the failure backoff.
    fn sample_oauth(&self, now_ms: i64) -> Option<UsageSnapshot> {
        if !self.oauth_enabled {
            return None;
        }
        {
            let until = self
                .oauth_backoff_until_ms
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if now_ms < *until {
                return None;
            }
        }
        match super::usage_oauth::fetch(&self.home) {
            Some(usage) => {
                let tier = read_account_tier(&self.home.join(".claude.json")).unwrap_or_default();
                Some(UsageSnapshot {
                    five_hour: UsageWindow {
                        used_tokens: usage.five_hour.utilization,
                        budget_tokens: 100.0,
                        resets_at_ms: usage.five_hour.resets_at_ms,
                    },
                    weekly: UsageWindow {
                        used_tokens: usage.seven_day.utilization,
                        budget_tokens: 100.0,
                        resets_at_ms: usage.seven_day.resets_at_ms,
                    },
                    tier,
                    source: UsageSource::AccountApi,
                })
            }
            None => {
                let mut until = self
                    .oauth_backoff_until_ms
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *until = now_ms + OAUTH_BACKOFF_MS;
                None
            }
        }
    }

    /// Test-only inspector for the eviction contract.
    #[cfg(test)]
    fn cached_file_count(&self) -> usize {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl UsageProbe for SessionLogUsageProbe {
    fn sample(&self) -> Option<UsageSnapshot> {
        let now = now_unix_ms();
        if let Some(snapshot) = self.sample_oauth(now) {
            return Some(snapshot);
        }
        let tier = read_account_tier(&self.home.join(".claude.json"))?;
        let budget = budget_for_tier(&tier)?;

        let projects_dir = self.home.join(".claude").join("projects");
        let logs = session_logs_within(&projects_dir, now);

        // Decide what needs re-parsing under a short lock, parse with the
        // lock RELEASED (tally_file reads whole files), then merge + evict
        // under a second short lock — `sample()` never blocks a concurrent
        // caller for the duration of file IO.
        let stale: Vec<(PathBuf, i64, u64)> = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            logs.iter()
                .filter(|(path, mtime_ms, len)| {
                    !cache
                        .get(path)
                        .is_some_and(|t| t.mtime_ms == *mtime_ms && t.len == *len)
                })
                .cloned()
                .collect()
        };
        let fresh: Vec<(PathBuf, i64, u64, HourBuckets)> = stale
            .into_iter()
            .map(|(path, mtime_ms, len)| {
                let buckets = tally_file(&path);
                (path, mtime_ms, len, buckets)
            })
            .collect();

        let seen: std::collections::HashSet<&PathBuf> = logs.iter().map(|(p, _, _)| p).collect();
        let mut merged: HourBuckets = BTreeMap::new();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (path, mtime_ms, len, buckets) in fresh {
            cache.insert(
                path,
                FileTally {
                    mtime_ms,
                    len,
                    buckets,
                },
            );
        }
        cache.retain(|p, _| seen.contains(p));
        for (path, _, _) in &logs {
            if let Some(t) = cache.get(path) {
                for (&h, &v) in &t.buckets {
                    *merged.entry(h).or_insert(0.0) += v;
                }
            }
        }
        drop(cache);

        Some(UsageSnapshot {
            five_hour: five_hour_window(&merged, now, budget.block_tokens),
            weekly: weekly_window(&merged, now, budget.weekly_tokens),
            tier,
            source: UsageSource::LocalEstimate,
        })
    }
}

/// The account's rate-limit tier slug from the CLI config, if present.
fn read_account_tier(config_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(config_path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.pointer("/oauthAccount/organizationRateLimitTier")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Every `.jsonl` under `projects/*/` modified within the scan horizon,
/// as `(path, mtime_ms, len)`. Missing dir → empty (no CLI use yet).
fn session_logs_within(projects_dir: &Path, now_ms: i64) -> Vec<(PathBuf, i64, u64)> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return out;
    };
    for project in projects.flatten() {
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let mtime_ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if now_ms.saturating_sub(mtime_ms) > SCAN_HORIZON_MS {
                continue;
            }
            out.push((path, mtime_ms, meta.len()));
        }
    }
    out
}

/// Tally one log file into hour buckets, line by line, deduplicated by
/// message id. Unparseable lines (drifted format, truncation) are skipped
/// — never an error.
///
/// Dedup matters: streaming re-emits the SAME assistant message (same
/// `message.id`, same API call) on multiple consecutive lines as content
/// blocks arrive — up to ~8 repeats per message in real logs, inflating a
/// naive line-sum ~5×. Last occurrence wins (it carries the final
/// `output_tokens`). Lines without an id (format drift) are counted
/// directly rather than dropped.
fn tally_file(path: &Path) -> HourBuckets {
    let mut buckets = HourBuckets::new();
    let Ok(f) = fs::File::open(path) else {
        return buckets;
    };
    let mut by_id: HashMap<String, (i64, f64)> = HashMap::new();
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        if let Some(event) = usage_event_from_line(&line) {
            match event.message_id {
                Some(id) => {
                    by_id.insert(id, (event.ts_ms, event.tokens));
                }
                None => {
                    *buckets.entry(bucket_key(event.ts_ms)).or_insert(0.0) += event.tokens;
                }
            }
        }
    }
    for (ts_ms, tokens) in by_id.into_values() {
        *buckets.entry(bucket_key(ts_ms)).or_insert(0.0) += tokens;
    }
    buckets
}

/// One counted usage event from a log line.
struct UsageEvent {
    message_id: Option<String>,
    ts_ms: i64,
    tokens: f64,
}

/// Extract a [`UsageEvent`] from one log line: assistant entries with a
/// `message.usage` object. Everything else → `None`.
fn usage_event_from_line(line: &str) -> Option<UsageEvent> {
    // Cheap pre-filter: every counted line mentions both markers.
    if !line.contains("\"assistant\"") || !line.contains("\"usage\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let ts_ms = parse_timestamp_ms(v.get("timestamp").and_then(Value::as_str)?)?;
    let usage = v.pointer("/message/usage")?;
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let model = v
        .pointer("/message/model")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message_id = v
        .pointer("/message/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(UsageEvent {
        message_id,
        ts_ms,
        tokens: weighted_tokens(input, output, model),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assistant_usage_line(ts: &str, model: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
        )
    }

    fn assistant_usage_line_with_id(
        id: &str,
        ts: &str,
        model: &str,
        input: u64,
        output: u64,
    ) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
        )
    }

    fn recent_ts(minutes_ago: i64) -> String {
        let ms = now_unix_ms() - minutes_ago * 60_000;
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn fixture_home(tier: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            format!(r#"{{"oauthAccount":{{"organizationRateLimitTier":"{tier}"}}}}"#),
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects/-w-x")).unwrap();
        home
    }

    fn write_log(home: &Path, name: &str, lines: &[String]) {
        let p = home.join(".claude/projects/-w-x").join(name);
        let mut f = std::fs::File::create(p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn usage_event_parses_assistant_usage() {
        let line = assistant_usage_line("2026-06-12T00:30:00Z", "claude-sonnet-4-6", 100, 20);
        let event = usage_event_from_line(&line).unwrap();
        assert_eq!(event.ts_ms, 1781222400000 + 30 * 60_000);
        assert_eq!(event.tokens, 120.0);
    }

    #[test]
    fn usage_event_skips_user_lines_and_garbage() {
        assert!(usage_event_from_line(r#"{"type":"user","message":{}}"#).is_none());
        assert!(usage_event_from_line("not json at all").is_none());
        assert!(usage_event_from_line("").is_none());
    }

    #[test]
    fn tally_dedupes_streamed_repeats_by_message_id() {
        // Streaming re-emits the same message id with growing
        // output_tokens; only the LAST occurrence may count.
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let ts = "2026-06-12T00:30:00Z";
        let lines = [
            assistant_usage_line_with_id("msg_a", ts, "claude-sonnet-4-6", 100, 5),
            assistant_usage_line_with_id("msg_a", ts, "claude-sonnet-4-6", 100, 15),
            assistant_usage_line_with_id("msg_a", ts, "claude-sonnet-4-6", 100, 40),
            assistant_usage_line_with_id("msg_b", ts, "claude-sonnet-4-6", 50, 10),
        ];
        let mut f = std::fs::File::create(&p).unwrap();
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
        let buckets = tally_file(&p);
        let total: f64 = buckets.values().sum();
        // msg_a counts once at its final shape (100+40), msg_b once (50+10).
        assert_eq!(total, 200.0);
    }

    #[test]
    fn tally_counts_idless_lines_directly() {
        // Format drift without `message.id` must still count (once per
        // line) rather than vanish.
        let home = tempfile::tempdir().unwrap();
        let p = home.path().join("s.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            "{}",
            assistant_usage_line("2026-06-12T00:30:00Z", "claude-sonnet-4-6", 10, 5)
        )
        .unwrap();
        let buckets = tally_file(&p);
        assert_eq!(buckets.values().sum::<f64>(), 15.0);
    }

    #[test]
    fn usage_event_applies_opus_multiplier() {
        let line = assistant_usage_line("2026-06-12T00:30:00Z", "claude-opus-4-8", 100, 0);
        let event = usage_event_from_line(&line).unwrap();
        assert_eq!(event.tokens, 500.0);
    }

    #[test]
    fn sample_none_without_account_config() {
        let home = tempfile::tempdir().unwrap();
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        assert!(probe.sample().is_none());
    }

    #[test]
    fn sample_none_for_unknown_tier() {
        let home = fixture_home("enterprise_unbounded");
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        assert!(probe.sample().is_none());
    }

    #[test]
    fn sample_tallies_recent_logs_into_active_block() {
        let home = fixture_home("default_claude_max_5x");
        write_log(
            home.path(),
            "s1.jsonl",
            &[
                assistant_usage_line(&recent_ts(30), "claude-sonnet-4-6", 1000, 200),
                assistant_usage_line(&recent_ts(10), "claude-sonnet-4-6", 500, 100),
                r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            ],
        );
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        let snap = probe.sample().expect("snapshot");
        assert_eq!(snap.tier, "default_claude_max_5x");
        assert_eq!(snap.five_hour.used_tokens, 1800.0);
        assert_eq!(snap.five_hour.budget_tokens, 220_000.0);
        assert!(snap.five_hour.resets_at_ms.is_some());
        assert_eq!(snap.weekly.used_tokens, 1800.0);
        assert_eq!(snap.weekly.budget_tokens, 33_000_000.0);
    }

    #[test]
    fn sample_reuses_cache_for_unchanged_files() {
        let home = fixture_home("default_claude_max_5x");
        write_log(
            home.path(),
            "s1.jsonl",
            &[assistant_usage_line(
                &recent_ts(5),
                "claude-sonnet-4-6",
                100,
                0,
            )],
        );
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        let first = probe.sample().expect("first");
        let second = probe.sample().expect("second");
        assert_eq!(first.five_hour.used_tokens, second.five_hour.used_tokens);
        assert_eq!(second.five_hour.used_tokens, 100.0);
    }

    #[test]
    fn sample_evicts_files_outside_scan_horizon() {
        let home = fixture_home("default_claude_max_5x");
        write_log(
            home.path(),
            "fresh.jsonl",
            &[assistant_usage_line(
                &recent_ts(5),
                "claude-sonnet-4-6",
                100,
                0,
            )],
        );
        write_log(
            home.path(),
            "ancient.jsonl",
            &[assistant_usage_line(
                &recent_ts(5),
                "claude-sonnet-4-6",
                999,
                0,
            )],
        );
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        assert!(probe.sample().is_some());
        assert_eq!(probe.cached_file_count(), 2);
        // Age `ancient.jsonl` past the horizon; the next sample must both
        // exclude its tokens and drop its cache entry.
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_millis((SCAN_HORIZON_MS + 60_000) as u64);
        let f = std::fs::File::options()
            .write(true)
            .open(home.path().join(".claude/projects/-w-x/ancient.jsonl"))
            .unwrap();
        f.set_modified(old).unwrap();
        let snap = probe.sample().expect("snapshot");
        assert_eq!(snap.five_hour.used_tokens, 100.0);
        assert_eq!(probe.cached_file_count(), 1);
    }

    #[test]
    fn sample_with_no_logs_reads_zero() {
        let home = fixture_home("claude_pro");
        let probe = SessionLogUsageProbe::new_estimate_only(home.path().to_path_buf());
        let snap = probe.sample().expect("snapshot");
        assert_eq!(snap.five_hour.used_tokens, 0.0);
        assert_eq!(snap.five_hour.resets_at_ms, None);
        assert_eq!(snap.weekly.used_tokens, 0.0);
    }
}
