//! Starting and ending an agent's driver session.
//!
//! A session is where the driver's own capture policy lives, and the policy is
//! chosen once: `capture_scope` is documented **"Immutable for the live
//! session."** OxiMux pins `window`, which has no escalation path at all —
//! `escalate_session` only unlocks a session that started as `auto`. So a
//! window-pinned session cannot reach desktop-scope tools even if every check
//! in [`crate::policy`] were removed.
//!
//! # Two things that constrain when this can run
//!
//! `cua-driver call` requires a daemon that is **already running**, and refuses
//! rather than starting one. The daemon comes up when the agent's own MCP proxy
//! launches it, which is after OxiMux has spawned the agent — so a session
//! cannot be created at spawn time, only once the agent has reached the driver.
//!
//! And the driver's `mcp` mode takes no `--session` flag in 0.12.6, so there is
//! no host-side way to bind the agent's proxy to a session id. The agent has to
//! pass `session` per call, which means it has to be told the id. Until that
//! delivery exists, calls arrive session-less and this pinning binds nothing;
//! [`crate::policy`] is then the only guard, and it is written to stand alone
//! for exactly that reason.

use std::path::Path;
use std::time::Duration;

use crate::exec::run_bounded;
use crate::session::{SessionId, SessionLedger};
use crate::Error;

/// Ceiling on a session start or end. A local socket round-trip; short because
/// the caller may be on a thread the user is waiting on.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The capture policy OxiMux gives every agent.
///
/// `window` rather than the driver's `auto` default: `auto` starts window-only
/// but *can* be escalated to the whole desktop, and `window` cannot. The
/// difference only shows up when something else has already gone wrong, which
/// is when it matters.
pub const CAPTURE_SCOPE: &str = "window";

/// The `start_session` arguments for an agent.
///
/// Split out from the call so the payload is assertable without a daemon —
/// getting `capture_scope` wrong is unrecoverable for the session's lifetime,
/// so it is worth testing rather than trusting.
pub fn start_payload(id: &SessionId) -> serde_json::Value {
    serde_json::json!({
        "session": id.as_str(),
        "capture_scope": CAPTURE_SCOPE,
    })
}

/// Declare a session, recording it before the driver is asked.
///
/// Order matters: a crash between recording and the call leaves a stale ledger
/// entry, which the next launch harmlessly tries to revoke. A crash the other
/// way round leaks a live session nothing knows to clean up.
pub fn start(driver: &Path, ledger: &SessionLedger, id: &SessionId) -> Result<(), Error> {
    let _ = ledger.record(id);
    let payload = start_payload(id).to_string();
    let out = run_bounded(
        driver,
        &["call", "start_session", "--json", &payload],
        SESSION_TIMEOUT,
    )?;
    if !out.success() {
        return Err(Error::SessionCommandFailed {
            command: "start_session",
            detail: first_line(&out.stderr, &out.stdout),
        });
    }
    Ok(())
}

/// End a session and drop it from the ledger.
///
/// The ledger entry is dropped even when the driver refuses, for the same
/// reason boot reconciliation clears unrevokable ids: a session the driver will
/// not end now will not become endable later, and retrying forever is worse
/// than forgetting.
pub fn end(driver: &Path, ledger: &SessionLedger, id: &SessionId) -> Result<(), Error> {
    let payload = serde_json::json!({ "session": id.as_str() }).to_string();
    let result = run_bounded(
        driver,
        &["call", "end_session", "--json", &payload],
        SESSION_TIMEOUT,
    );
    let _ = ledger.forget(id);
    match result {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(Error::SessionCommandFailed {
            command: "end_session",
            detail: first_line(&out.stderr, &out.stdout),
        }),
        Err(err) => Err(err),
    }
}

/// The first non-empty line of stderr, falling back to stdout — the driver
/// reports refusals on either depending on how far the call got.
fn first_line(stderr: &str, stdout: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no detail")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_pinned_to_window_scope() {
        // The structural half of the safety model. `auto` would leave an
        // escalation path open and is the driver's default, so an omitted or
        // mistyped field silently gets the weaker policy.
        let payload = start_payload(&SessionId::for_agent("chat-a"));
        assert_eq!(payload["capture_scope"], "window");
        assert_eq!(payload["session"], "oximux-chat-a");
    }

    #[test]
    fn the_pinned_scope_is_one_the_driver_cannot_escalate() {
        // `escalate_session` unlocks the desktop phase of an `auto` session
        // only. Pinning anything else would reopen that door.
        assert_ne!(CAPTURE_SCOPE, "auto");
        assert_ne!(CAPTURE_SCOPE, "desktop");
    }

    #[test]
    fn starting_records_the_session_before_the_driver_can_fail() {
        // The crash-safety ordering: even when the call fails outright, the id
        // is on disk for boot reconciliation to clean up.
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SessionLedger::in_data_dir(dir.path());
        let id = SessionId::for_agent("chat-a");

        let err = start(Path::new("/nonexistent/cua-driver"), &ledger, &id)
            .expect_err("a missing driver must fail");
        assert!(matches!(err, Error::Spawn { .. }), "{err:?}");
        assert!(ledger.outstanding().expect("read").contains(&id));
    }

    #[test]
    fn ending_forgets_the_session_even_when_the_driver_refuses() {
        // Otherwise every launch retries an id the driver will never accept.
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SessionLedger::in_data_dir(dir.path());
        let id = SessionId::for_agent("chat-a");
        ledger.record(&id).expect("record");

        let _ = end(Path::new("/nonexistent/cua-driver"), &ledger, &id);
        assert!(ledger.outstanding().expect("read").is_empty());
    }

    #[test]
    fn ending_succeeds_against_a_driver_that_accepts_it() {
        // /usr/bin/true takes any arguments and exits 0, standing in for a
        // driver that ended the session.
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SessionLedger::in_data_dir(dir.path());
        let id = SessionId::for_agent("chat-a");
        ledger.record(&id).expect("record");

        end(Path::new("/usr/bin/true"), &ledger, &id).expect("end");
        assert!(ledger.outstanding().expect("read").is_empty());
    }

    #[test]
    fn a_refusal_carries_the_drivers_own_words() {
        // /usr/bin/false exits non-zero with no output, so this also covers the
        // no-detail fallback rather than producing an empty message.
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SessionLedger::in_data_dir(dir.path());
        let err = end(
            Path::new("/usr/bin/false"),
            &ledger,
            &SessionId::for_agent("chat-a"),
        )
        .expect_err("non-zero exit must fail");
        assert!(err.to_string().contains("end_session"), "{err}");
    }
}
