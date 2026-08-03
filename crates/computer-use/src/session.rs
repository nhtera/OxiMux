//! Per-agent driver sessions, and cleaning up the ones a crash left behind.
//!
//! A driver session is the unit that makes N agents driving N apps possible:
//! each declares its own id, which owns that run's agent cursor and per-session
//! capture policy. One OxiMux agent chat maps to one session.
//!
//! Sessions are also the *only* thing OxiMux may clean up. The daemon is shared
//! with every other MCP client on the machine (see [`crate::daemon`]), so the
//! recovery story is not "kill orphaned daemons" but "revoke the sessions we
//! opened and never closed". The ledger exists because that has to survive a
//! `kill -9`, which runs no `Drop` and no quit handler.
//!
//! # Why this is not simplified on Windows
//!
//! The obligation above assumes sessions **outlive** the `mcp` proxy, against a
//! daemon that keeps running. If Windows instead ran a per-agent runtime, that
//! state would die with the proxy and reconciliation would have nothing to do.
//!
//! The driver's CLI says it does not: `--direct` is documented as "own the
//! runtime in this MCP process", so the default proxy path does not, and the
//! daemon it launches outlives it. That matches macOS, for an unrelated reason
//! — Session 0 isolation rather than TCC attribution (see [`crate::daemon`]).
//!
//! But that is read off `--help`, not observed. Confirming it means launching
//! `cua-driver mcp`, which starts something that can drive the screen, and that
//! is not a thing to do incidentally.
//!
//! So this stays as-is, deliberately, and the asymmetry is what decides it. If
//! sessions really do die with the proxy, reconciling anyway costs a few
//! `revoke` calls against ids that no longer exist. Those land in
//! [`Reconciliation::failed`] rather than `revoked` if the driver exits
//! non-zero for an unknown id — a reporting wrinkle, not a leak, because
//! [`reconcile`] clears the ledger either way and will not retry them forever.
//!
//! The opposite mistake costs a live session nobody can see and nothing will
//! clean up. A no-op reconcile that *looks* like it cleans up would be worse
//! than none at all.
//!
//! Worth measuring when the posture is confirmed: if `revoke` on an unknown id
//! does exit non-zero, a Windows user could see a startup report of "failed to
//! revoke" for sessions that were never leaked. That is a cosmetic fix to make
//! *then*, against a known exit code, rather than a guess to encode now.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::run_bounded;
use crate::Error;

/// File under the app data dir holding not-yet-ended session ids.
pub const LEDGER_FILE_NAME: &str = "computer-use-sessions.json";

/// Ceiling on one revoke. Short: it is a local socket round-trip, and boot
/// reconciliation walks the whole ledger before the UI is useful.
pub const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a derived id, so a pathological agent session id cannot produce an
/// unreadable cursor label.
const MAX_ID_LEN: usize = 48;

/// A driver session id. Visible to the user — it labels the agent cursor — so
/// it is readable rather than opaque.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Derive the session id for an agent chat.
    ///
    /// Sanitised because the id crosses a process boundary as a CLI argument
    /// and is echoed into an on-screen label; only `[A-Za-z0-9_-]` survives.
    pub fn for_agent(agent_session_id: &str) -> Self {
        let cleaned: String = agent_session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = cleaned.trim_matches('-');
        let body = if trimmed.is_empty() {
            "session".to_string()
        } else {
            trimmed.chars().take(MAX_ID_LEN).collect()
        };
        Self(format!("oximux-{body}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// On-disk record of sessions OxiMux has opened and not yet ended.
#[derive(Debug, Clone)]
pub struct SessionLedger {
    path: PathBuf,
}

impl SessionLedger {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Default location, next to the rest of the app's on-disk state.
    pub fn in_data_dir(app_data_dir: &Path) -> Self {
        Self::at(app_data_dir.join(LEDGER_FILE_NAME))
    }

    /// Note a session as live. Must happen *before* the agent can use it: a
    /// crash between recording and first use leaves a harmless stale entry,
    /// whereas a crash between first use and recording leaks a real session.
    pub fn record(&self, id: &SessionId) -> std::io::Result<()> {
        let mut ids = self.outstanding()?;
        if ids.insert(id.clone()) {
            self.write(&ids)?;
        }
        Ok(())
    }

    /// Drop a session that was ended cleanly.
    pub fn forget(&self, id: &SessionId) -> std::io::Result<()> {
        let mut ids = self.outstanding()?;
        if ids.remove(id) {
            self.write(&ids)?;
        }
        Ok(())
    }

    /// Sessions believed live. A missing or corrupt ledger reads as empty —
    /// this runs on the boot path, and refusing to start over an unparseable
    /// cleanup file would be a worse failure than skipping the cleanup.
    pub fn outstanding(&self) -> std::io::Result<BTreeSet<SessionId>> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(err) => return Err(err),
        };
        Ok(serde_json::from_str::<Vec<String>>(&raw)
            .unwrap_or_default()
            .into_iter()
            .map(SessionId)
            .collect())
    }

    /// Write via temp-then-rename so a crash mid-write cannot leave a
    /// half-written ledger that loses every id at once.
    fn write(&self, ids: &BTreeSet<SessionId>) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let payload = serde_json::to_string(&ids.iter().map(|id| &id.0).collect::<Vec<_>>())
            .map_err(std::io::Error::other)?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, payload)?;
        std::fs::rename(&temp, &self.path)
    }
}

/// Outcome of a boot reconciliation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciliation {
    pub revoked: Vec<SessionId>,
    /// Sessions the driver would not revoke. Reported rather than retried —
    /// the usual cause is the daemon having restarted, which already dropped
    /// them.
    pub failed: Vec<SessionId>,
}

impl Reconciliation {
    pub fn is_empty(&self) -> bool {
        self.revoked.is_empty() && self.failed.is_empty()
    }
}

/// End every session the ledger still lists, then clear it.
///
/// Runs on OxiMux launch. Scoped to recorded ids — never `revoke --all`, which
/// would also kill sessions belonging to other MCP clients sharing the daemon.
pub fn reconcile(
    driver: &Path,
    ledger: &SessionLedger,
    timeout: Duration,
) -> Result<Reconciliation, Error> {
    let outstanding = ledger.outstanding().unwrap_or_default();
    let mut result = Reconciliation::default();

    for id in outstanding {
        match run_bounded(driver, &["revoke", "--session", id.as_str()], timeout) {
            Ok(out) if out.success() => result.revoked.push(id),
            Ok(_) | Err(_) => result.failed.push(id),
        }
    }

    // Cleared either way: a session that could not be revoked will not become
    // revocable later, and keeping it would retry forever on every launch.
    if !result.is_empty() {
        let _ = ledger.write(&BTreeSet::new());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (tempfile::TempDir, SessionLedger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SessionLedger::in_data_dir(dir.path());
        (dir, ledger)
    }

    #[test]
    fn derives_a_readable_prefixed_id() {
        assert_eq!(
            SessionId::for_agent("abc123").as_str(),
            "oximux-abc123"
        );
    }

    #[test]
    fn sanitises_characters_that_cross_a_process_boundary() {
        // The id becomes a CLI argument and an on-screen label.
        assert_eq!(
            SessionId::for_agent("a b/c;rm -rf").as_str(),
            "oximux-a-b-c-rm--rf"
        );
        assert!(!SessionId::for_agent("../../etc/passwd")
            .as_str()
            .contains('/'));
    }

    #[test]
    fn an_empty_or_all_punctuation_id_still_yields_a_usable_session() {
        assert_eq!(SessionId::for_agent("").as_str(), "oximux-session");
        assert_eq!(SessionId::for_agent("///").as_str(), "oximux-session");
    }

    #[test]
    fn caps_length() {
        let long = "x".repeat(500);
        let id = SessionId::for_agent(&long);
        assert_eq!(id.as_str().len(), "oximux-".len() + MAX_ID_LEN);
    }

    #[test]
    fn distinct_agents_get_distinct_sessions() {
        // The parallelism premise: two agents must never share a session.
        assert_ne!(SessionId::for_agent("agent-a"), SessionId::for_agent("agent-b"));
    }

    #[test]
    fn a_missing_ledger_reads_as_empty() {
        let (_dir, ledger) = ledger();
        assert!(ledger.outstanding().expect("read").is_empty());
    }

    #[test]
    fn records_and_forgets_round_trip() {
        let (_dir, ledger) = ledger();
        let a = SessionId::for_agent("a");
        let b = SessionId::for_agent("b");
        ledger.record(&a).expect("record a");
        ledger.record(&b).expect("record b");
        assert_eq!(ledger.outstanding().expect("read").len(), 2);

        ledger.forget(&a).expect("forget a");
        let left = ledger.outstanding().expect("read");
        assert!(!left.contains(&a));
        assert!(left.contains(&b));
    }

    #[test]
    fn recording_twice_does_not_duplicate() {
        let (_dir, ledger) = ledger();
        let a = SessionId::for_agent("a");
        ledger.record(&a).expect("first");
        ledger.record(&a).expect("second");
        assert_eq!(ledger.outstanding().expect("read").len(), 1);
    }

    #[test]
    fn a_corrupt_ledger_does_not_block_boot() {
        // Reconciliation runs on the launch path; an unparseable cleanup file
        // must degrade to "nothing to clean", not to a failed start.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LEDGER_FILE_NAME);
        std::fs::write(&path, "{ not json").expect("write");
        assert!(SessionLedger::at(&path).outstanding().expect("read").is_empty());
    }

    #[test]
    fn a_session_surviving_a_crash_is_still_listed() {
        // Simulates kill -9: nothing ran forget(), so the id is still there for
        // the next launch to revoke.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = SessionId::for_agent("crashed");
        SessionLedger::in_data_dir(dir.path())
            .record(&id)
            .expect("record");

        let after_restart = SessionLedger::in_data_dir(dir.path());
        assert!(after_restart.outstanding().expect("read").contains(&id));
    }

    #[test]
    fn reconcile_with_nothing_outstanding_runs_no_commands() {
        // A driver path that would fail if invoked proves no revoke was tried.
        let (_dir, ledger) = ledger();
        let result = reconcile(
            Path::new("/nonexistent/cua-driver"),
            &ledger,
            Duration::from_millis(200),
        )
        .expect("reconcile");
        assert!(result.is_empty());
    }

    #[test]
    fn reconcile_clears_the_ledger_even_when_revoke_fails() {
        // Otherwise an unrevokable id is retried on every launch forever.
        let (_dir, ledger) = ledger();
        ledger.record(&SessionId::for_agent("stale")).expect("record");

        let result = reconcile(
            Path::new("/nonexistent/cua-driver"),
            &ledger,
            Duration::from_millis(200),
        )
        .expect("reconcile");

        assert_eq!(result.failed.len(), 1);
        assert!(result.revoked.is_empty());
        assert!(ledger.outstanding().expect("read").is_empty());
    }

    #[test]
    fn reconcile_reports_revoked_sessions() {
        let (_dir, ledger) = ledger();
        ledger.record(&SessionId::for_agent("live")).expect("record");
        // A program that accepts any arguments and exits 0, standing in for a
        // driver that revoked successfully.
        let stub = crate::fixtures::always_succeeds();
        let result =
            reconcile(&stub, &ledger, Duration::from_secs(5)).expect("reconcile");
        assert_eq!(result.revoked.len(), 1);
        assert!(result.failed.is_empty());
        assert!(ledger.outstanding().expect("read").is_empty());
    }
}
