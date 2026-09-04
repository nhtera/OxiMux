//! Usage windows recorded by the Codex CLI in its session rollout logs.
//!
//! Unlike the primary CLI, this one publishes no usage endpoint — but it does
//! not have to. Every turn the server answers with the account's rate-limit
//! windows, and the CLI journals that answer verbatim into its session
//! rollout: a `token_count` event carrying `rate_limits.primary` /
//! `.secondary`, the same numbers its own `/status` renders. Reading them back
//! costs one bounded file read, no network call, and no credential — the token
//! is never touched, so none of the account-safety rules around it apply here.
//!
//! # The catch, and the rule that answers it
//!
//! A reading off disk is **never live**. It is exactly as old as the last turn
//! the user took, which may be minutes or days. That is fine while the window
//! it describes is still running — the number was true when the server sent it
//! and nothing since has consumed from a window the user hasn't touched. It
//! stops being fine the moment the window *resets*: the recorded percentage
//! then describes a period that no longer exists. So every window is checked
//! against its own `resets_at` and dropped once it passes
//! ([`UsageWindow::is_current`]). A reading with nothing left is reported
//! unavailable rather than shown — the alternative is printing a number that
//! is no longer true, which is the one thing this meter refuses to do.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::usage::{UsageSnapshot, UsageWindow};
use super::{parse_timestamp_ms, read_tail};

/// Directory under the CLI's home holding the rollout logs.
const SESSIONS_DIR: &str = "sessions";

/// How many recent rollout files to open before giving up. A session that
/// ended before its first server response carries no rate-limit line at all,
/// so the newest file is not always the one with a reading — but reading the
/// whole history to find one would cost more than the meter is worth.
const MAX_FILES_SCANNED: usize = 5;

/// Trailing bytes read per rollout. The rate-limit line lands after every
/// turn, so the last one sits near the end of even a very long session.
const TAIL_BYTES: u64 = 256 * 1024;

/// The CLI's state root: `$CODEX_HOME` if set, else `<home>/.codex`.
///
/// Honours the override for the same reason the hook installer does — a user
/// who relocated it would otherwise get a meter that reads a directory they
/// stopped writing to, and silently reports the usage of months ago.
pub fn state_dir(home: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".codex"),
    }
}

/// Whether this CLI has ever run on this machine.
///
/// The meter shows a row per account the user actually has. A machine that
/// never ran this CLI gets no row at all — not a row reporting a failure,
/// which would read as something being broken rather than absent.
pub fn is_configured(home: &Path) -> bool {
    sessions_dir_at(&state_dir(home)).is_dir()
}

/// The newest usable reading, or the reason there isn't one.
pub fn fetch(home: &Path, now_ms: i64) -> Result<UsageSnapshot, String> {
    fetch_at(&sessions_dir_at(&state_dir(home)), now_ms)
}

/// Where the rollouts live under a state root.
fn sessions_dir_at(state_dir: &Path) -> PathBuf {
    state_dir.join(SESSIONS_DIR)
}

/// The path-taking half of [`fetch`].
///
/// Split out because [`state_dir`] resolves `CODEX_HOME` at call time, and an
/// environment variable is shared by every thread in the test binary — a test
/// that set one to point at a fixture would be read by whichever other test
/// happened to run beside it. Tests address a directory instead.
fn fetch_at(sessions: &Path, now_ms: i64) -> Result<UsageSnapshot, String> {
    let candidates = recent_rollouts(sessions, MAX_FILES_SCANNED);
    if candidates.is_empty() {
        return Err("No sessions recorded yet".to_string());
    }

    let Some(snapshot) = candidates
        .iter()
        .find_map(|path| read_tail(path, TAIL_BYTES).and_then(|t| newest_reading(&t)))
    else {
        return Err("No usage reported in recent sessions".to_string());
    };

    current_windows_only(snapshot, now_ms)
}

/// Drop windows that have reset since the reading was captured, and refuse the
/// whole reading when none survive. See the module docs for why this is the
/// honest answer rather than showing the recorded percentages anyway.
fn current_windows_only(mut snapshot: UsageSnapshot, now_ms: i64) -> Result<UsageSnapshot, String> {
    snapshot.windows.retain(|w| w.is_current(now_ms));
    if snapshot.windows.is_empty() {
        return Err("Every window has reset since the last session".to_string());
    }
    Ok(snapshot)
}

/// The last rate-limit reading in a chunk of rollout text.
///
/// Scans backwards so the newest turn wins, and tolerates a partial first line
/// (the tail read starts mid-file) by simply failing to parse it.
fn newest_reading(tail: &str) -> Option<UsageSnapshot> {
    tail.lines()
        .rev()
        .filter(|l| l.contains("rate_limits"))
        .find_map(parse_reading)
}

/// Parse one rollout line into a reading. Pure — unit-tested against a
/// captured fixture. `None` for any line that isn't a rate-limit-bearing
/// event, so format drift costs the meter rather than crashing the app.
fn parse_reading(line: &str) -> Option<UsageSnapshot> {
    let v: Value = serde_json::from_str(line).ok()?;
    let limits = v.pointer("/payload/rate_limits")?;

    // Shortest span first, matching the order the popover renders them.
    let windows: Vec<UsageWindow> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| parse_window(limits.get(k)))
        .collect();
    if windows.is_empty() {
        return None;
    }

    // The envelope's own timestamp is when the server said this — the reading
    // is exactly that old, and the UI discloses it.
    let captured_at_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp_ms);

    Some(UsageSnapshot {
        windows,
        tier: limits
            .get("plan_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        captured_at_ms,
    })
}

/// One window. `resets_at` is unix **seconds** here, unlike the millis used
/// everywhere else in the meter.
fn parse_window(v: Option<&Value>) -> Option<UsageWindow> {
    let v = v?;
    let utilization = v.get("used_percent").and_then(Value::as_f64)?;
    let window_minutes = v.get("window_minutes").and_then(Value::as_u64)? as u32;
    Some(UsageWindow {
        window_minutes,
        utilization: utilization.clamp(0.0, 100.0),
        resets_at_ms: v.get("resets_at").and_then(Value::as_i64).map(|s| s * 1_000),
    })
}

/// Up to `limit` rollout logs, newest first.
///
/// The layout is `sessions/<year>/<month>/<day>/rollout-<stamp>-<uuid>.jsonl`,
/// every component zero-padded — so descending lexicographic order *is*
/// reverse chronological order, at every level. Walking it that way reads a
/// handful of directory entries instead of stat-ing a history that grows
/// without bound.
fn recent_rollouts(sessions: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::with_capacity(limit);
    for year in children_desc(sessions) {
        for month in children_desc(&year) {
            for day in children_desc(&month) {
                for log in children_desc(&day) {
                    if log.extension().is_some_and(|e| e == "jsonl") {
                        found.push(log);
                        if found.len() == limit {
                            return found;
                        }
                    }
                }
            }
        }
    }
    found
}

/// A directory's entries sorted newest-name-first, or empty when unreadable.
fn children_desc(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::usage::{FIVE_HOUR_MINUTES, WEEK_MINUTES};

    /// Captured from a live rollout (2026-08-28), trimmed of the token counts
    /// the meter does not read. Extra and null fields must be tolerated.
    const FIXTURE: &str = r#"{"timestamp":"2026-08-28T09:01:37.476Z","ordinal":13,"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":6.0,"window_minutes":300,"resets_at":1787925683},"secondary":{"used_percent":23.0,"window_minutes":10080,"resets_at":1788458128},"credits":{"has_credits":false},"individual_limit":null,"plan_type":"plus","rate_limit_reached_type":null}}}"#;

    fn fixture_snapshot() -> UsageSnapshot {
        parse_reading(FIXTURE).expect("the captured fixture must parse")
    }

    #[test]
    fn parses_both_windows_from_a_live_line() {
        let snap = fixture_snapshot();
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.windows[0].window_minutes, FIVE_HOUR_MINUTES);
        assert_eq!(snap.windows[0].utilization, 6.0);
        assert_eq!(snap.windows[1].window_minutes, WEEK_MINUTES);
        assert_eq!(snap.windows[1].utilization, 23.0);
        assert_eq!(snap.tier, "plus");
    }

    #[test]
    fn reset_seconds_become_millis() {
        let snap = fixture_snapshot();
        assert_eq!(snap.windows[0].resets_at_ms, Some(1_787_925_683_000));
    }

    #[test]
    fn the_reading_carries_the_moment_it_was_captured() {
        // Never live: the UI must be able to say how old this is.
        let snap = fixture_snapshot();
        assert_eq!(snap.captured_at_ms, parse_timestamp_ms("2026-08-28T09:01:37.476Z"));
    }

    #[test]
    fn a_line_without_rate_limits_is_not_a_reading() {
        assert!(parse_reading(r#"{"type":"event_msg","payload":{"type":"agent_message"}}"#).is_none());
        assert!(parse_reading("not json").is_none());
        // Present but empty of windows — nothing to show, so not a reading.
        assert!(parse_reading(r#"{"payload":{"rate_limits":{"plan_type":"plus"}}}"#).is_none());
    }

    #[test]
    fn the_last_reading_in_the_file_wins() {
        let older = FIXTURE.replace(r#""used_percent":6.0"#, r#""used_percent":1.0"#);
        let tail = format!("{older}\n{{\"type\":\"chatter\"}}\n{FIXTURE}\n");
        assert_eq!(newest_reading(&tail).unwrap().windows[0].utilization, 6.0);
    }

    #[test]
    fn a_truncated_leading_line_is_skipped() {
        // `read_tail` starts mid-file, so the first line is usually a fragment.
        let tail = format!("_percent\":99.0,\"window_minutes\":300}}}}}}\n{FIXTURE}\n");
        assert_eq!(newest_reading(&tail).unwrap().windows[0].utilization, 6.0);
    }

    #[test]
    fn a_window_past_its_reset_is_dropped() {
        let snap = fixture_snapshot();
        // Just after the 5-hour window resets, before the weekly one does.
        let now = 1_787_925_683_000 + 1;
        let kept = current_windows_only(snap, now).unwrap();
        assert_eq!(kept.windows.len(), 1);
        assert_eq!(kept.windows[0].window_minutes, WEEK_MINUTES);
    }

    #[test]
    fn a_reading_with_every_window_reset_is_refused() {
        // Showing 6% for a window that ended days ago would be the fabricated
        // number the meter exists to refuse.
        let snap = fixture_snapshot();
        let err = current_windows_only(snap, 1_788_458_128_000 + 1).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn a_missing_sessions_dir_is_not_configured() {
        let root = tempfile::tempdir().unwrap();
        assert!(!sessions_dir_at(root.path()).is_dir());
        std::fs::create_dir_all(sessions_dir_at(root.path())).unwrap();
        assert!(sessions_dir_at(root.path()).is_dir());
    }

    #[test]
    fn state_dir_defaults_under_home() {
        // The `CODEX_HOME` branch is deliberately untested: it reads a process
        // -wide variable, and setting one here would leak into every other
        // test in this binary.
        assert_eq!(
            state_dir(Path::new("/home/u")),
            PathBuf::from("/home/u/.codex"),
        );
    }

    #[test]
    fn rollouts_come_back_newest_first_across_the_date_tree() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join(".codex").join(SESSIONS_DIR);
        for (y, m, d, name) in [
            ("2026", "08", "27", "rollout-2026-08-27T10-00-00-a.jsonl"),
            ("2026", "08", "28", "rollout-2026-08-28T02-00-00-b.jsonl"),
            ("2026", "08", "28", "rollout-2026-08-28T16-00-00-c.jsonl"),
            ("2025", "12", "31", "rollout-2025-12-31T23-00-00-d.jsonl"),
        ] {
            let dir = sessions.join(y).join(m).join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), "").unwrap();
        }
        let names: Vec<String> = recent_rollouts(&sessions, 10)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "rollout-2026-08-28T16-00-00-c.jsonl",
                "rollout-2026-08-28T02-00-00-b.jsonl",
                "rollout-2026-08-27T10-00-00-a.jsonl",
                "rollout-2025-12-31T23-00-00-d.jsonl",
            ]
        );
        assert_eq!(recent_rollouts(&sessions, 2).len(), 2, "the limit bounds the walk");
    }

    #[test]
    fn a_session_without_a_reading_falls_through_to_the_next() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex").join(SESSIONS_DIR).join("2026").join("08").join("28");
        std::fs::create_dir_all(&dir).unwrap();
        // Newest file: a session that ended before its first server answer.
        std::fs::write(dir.join("rollout-2026-08-28T18-00-00-b.jsonl"), "{\"type\":\"session_meta\"}\n").unwrap();
        std::fs::write(dir.join("rollout-2026-08-28T09-00-00-a.jsonl"), format!("{FIXTURE}\n")).unwrap();
        let snap = fetch_at(&sessions_dir_at(&home.path().join(".codex")), 1_787_925_000_000).unwrap();
        assert_eq!(snap.windows[0].utilization, 6.0);
    }

    #[test]
    fn no_sessions_at_all_reports_a_reason() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex").join(SESSIONS_DIR)).unwrap();
        let err = fetch_at(&sessions_dir_at(&home.path().join(".codex")), 0).unwrap_err();
        assert!(!err.is_empty());
    }
}

