//! Replacing a daemon left over from another app version, at boot.
//!
//! Every update leaves the previous version's daemon running (same protocol,
//! same socket), so without this the new app keeps using old daemon code until
//! the user restarts it by hand. A stale daemon is replaced only when it has no
//! sessions: the daemon's own `Shutdown { kill_sessions: false }` refuses while
//! any are alive, in one step with the check, so a session started in between
//! is never killed. A stale daemon with sessions is kept and reported.

use std::time::Duration;

use oximux_relay_client::RelayClient;
use oximux_relay_proto::PidRecord;

use crate::identity::{Stopped, stop_verified_daemon, wait_dead};
use crate::stop::request_shutdown;
use crate::{RelaySupervisor, SupervisorError};

/// How long an idle daemon that agreed to stop may take to exit.
const IDLE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long it may take to answer the shutdown request.
const IDLE_SHUTDOWN_RPC_TIMEOUT: Duration = Duration::from_secs(2);
/// Between SIGTERM and SIGKILL for one that agreed and then did not exit.
const LINGER_SIGNAL_GRACE: Duration = Duration::from_secs(2);

/// Whether the running daemon is from this app version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Staleness {
    Fresh,
    Stale { daemon_version: String },
    /// No readable pid record: nothing to decide on, so it is kept.
    Unknown,
}

/// Compare the daemon's recorded version with the app's. Version only: every
/// dev build shares the workspace version, so a dev daemon is never replaced
/// behind the user's back.
pub(crate) fn assess(record: Option<&PidRecord>, app_version: &str) -> Staleness {
    match record {
        None => Staleness::Unknown,
        Some(record) if record.version == app_version => Staleness::Fresh,
        Some(record) => Staleness::Stale { daemon_version: record.version.clone() },
    }
}

/// The daemon boot ends up with.
pub struct BootDaemon {
    pub client: RelayClient,
    /// Set when the daemon is from another app version and was kept.
    pub stale_version: Option<String>,
    /// The daemon's version could not be read, so whether it has this app's
    /// features is unknown. Callers that would rather answer twice than not at
    /// all (the program-status reply) treat it like a stale one.
    pub version_unknown: bool,
}

impl RelaySupervisor {
    /// Replace the daemon `client` is connected to when it is from another
    /// app version and has no sessions. `serve_attached` is asked only for a
    /// stale daemon: an `oximux serve` on it cannot reconnect, so it is kept.
    ///
    /// `Err` only when the old daemon agreed to go and no new one came up.
    pub async fn replace_if_stale_and_idle(
        &self,
        client: RelayClient,
        app_version: &str,
        serve_attached: impl FnOnce() -> bool,
    ) -> Result<BootDaemon, SupervisorError> {
        let record = self.read_pid_record();
        let daemon_version = match assess(record.as_ref(), app_version) {
            Staleness::Fresh => {
                return Ok(BootDaemon { client, stale_version: None, version_unknown: false });
            }
            Staleness::Unknown => {
                tracing::info!("relay pid record unreadable; not checking the daemon's version");
                return Ok(BootDaemon { client, stale_version: None, version_unknown: true });
            }
            Staleness::Stale { daemon_version } => daemon_version,
        };
        let kept = |client: RelayClient| -> Result<BootDaemon, SupervisorError> {
            Ok(BootDaemon {
                client,
                stale_version: Some(daemon_version.clone()),
                version_unknown: false,
            })
        };
        if serve_attached() {
            tracing::info!(%daemon_version, app_version, "stale relay daemon kept: oximux serve uses it");
            return kept(client);
        }
        // Who to wait for — and, should it linger, verify before signalling.
        let Some((pid, expect)) = self.expect_current() else {
            return kept(client);
        };
        if !request_shutdown(&client, false, IDLE_SHUTDOWN_RPC_TIMEOUT).await {
            tracing::info!(%daemon_version, app_version, "stale relay daemon kept: it has sessions");
            return kept(client);
        }
        drop(client);
        // One that agreed but is still around holds its socket and pid file,
        // and would unlink the new daemon's when it finally exits. It has no
        // sessions, so ending it costs nothing; one that cannot be verified or
        // ended leaves boot on in-process terminals rather than beside it.
        if !wait_dead(pid, IDLE_EXIT_TIMEOUT).await {
            tracing::warn!(pid, "stale relay daemon agreed to stop but is still running; signalling it");
            match stop_verified_daemon(pid, &expect, LINGER_SIGNAL_GRACE).await {
                Stopped::Stopped | Stopped::NotRunning => {}
                stopped => {
                    return Err(SupervisorError::Other(anyhow::anyhow!(
                        "stale relay daemon (pid {pid}) would not exit: {stopped:?}"
                    )));
                }
            }
        }
        let client = self.ensure_running().await.inspect_err(|err| {
            tracing::warn!(%err, "stale relay daemon stopped, but no new daemon came up");
        })?;
        tracing::info!(
            %daemon_version,
            app_version,
            old_pid = pid,
            new_pid = ?self.read_pid(),
            "replaced an idle relay daemon from another app version"
        );
        // The daemon binary shipped beside the app may itself be from another
        // version (an override, a mismatched bundle): still flag it.
        let stale_version = match assess(self.read_pid_record().as_ref(), app_version) {
            Staleness::Stale { daemon_version } => {
                tracing::warn!(%daemon_version, app_version, "the relay binary's version differs from the app's");
                Some(daemon_version)
            }
            Staleness::Fresh | Staleness::Unknown => None,
        };
        // Whatever replaced it was started by this app, so Unknown here is ours.
        Ok(BootDaemon { client, stale_version, version_unknown: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: &str) -> PidRecord {
        PidRecord { pid: 1, version: version.into(), started_at_epoch_secs: 0, exe: String::new() }
    }

    #[test]
    fn same_version_is_fresh() {
        assert_eq!(assess(Some(&record("0.1.33")), "0.1.33"), Staleness::Fresh);
    }

    #[test]
    fn another_version_is_stale() {
        assert_eq!(
            assess(Some(&record("0.1.32")), "0.1.33"),
            Staleness::Stale { daemon_version: "0.1.32".into() }
        );
    }

    #[test]
    fn a_missing_record_is_unknown() {
        assert_eq!(assess(None, "0.1.33"), Staleness::Unknown);
    }
}
