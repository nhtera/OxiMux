//! Reading daemon state — deliberately *not* controlling it.
//!
//! # OxiMux does not own the daemon
//!
//! The plan this phase came from assumed OxiMux would start, supervise, and
//! reap a `cua-driver serve` process on a private socket. Probing the shipped
//! driver showed that is wrong on all three counts, so this module reads state
//! and nothing else. What the driver actually does, verified by running it:
//!
//! - `cua-driver mcp` **starts the daemon itself** when it is not already up,
//!   reporting: "mcp launched without CuaDriver.app's TCC grants; auto-launching
//!   the daemon via `open -n -g -a CuaDriver --args serve`". `-g` keeps it
//!   background, so nothing steals focus.
//! - It launches through LaunchServices *on purpose*. macOS attributes TCC to
//!   the responsible process, and going through `open -a` makes that
//!   `CuaDriver.app` — a stable identity whose Accessibility and Screen
//!   Recording grants survive OxiMux rebuilds. (The "never launch via `open -a`"
//!   rule applies to the deferred *embedded* mode, where the point is to
//!   inherit OxiMux's own grants instead.)
//! - The socket is a fixed shared path, not per-host, and the daemon outlives
//!   the `mcp` proxy that started it. It is a machine-wide singleton serving
//!   every MCP client the user has configured.
//!
//! That last point is why there is no reap-on-quit here and no kqueue watch on
//! OxiMux's pid: killing the daemon on OxiMux exit would tear the driver out
//! from under the user's other agents. OxiMux's cleanup obligation is its own
//! *sessions* (see [`crate::session`]), which it can end without touching
//! anyone else's.
//!
//! # On Windows the conclusion survives but the reasoning does not
//!
//! Everything above is argued from TCC. None of that reasoning exists on
//! Windows, and leaving it to stand unqualified would read as an explanation
//! of behaviour it does not explain. What differs:
//!
//! - **There is no LaunchServices hop and no responsible process.** The
//!   `open -n -g -a CuaDriver` dance exists so macOS attributes Accessibility
//!   and Screen Recording to a stable app identity. Windows has no such
//!   attribution, because it has no grant to attribute — input synthesis is
//!   ambient to any process in the interactive session.
//! - **The daemon solves a different problem: Session 0 isolation.** A process
//!   started outside the interactive session (over SSH, from a service) cannot
//!   touch the desktop at all. A daemon living *in* the user's session is how
//!   the driver reaches it, which is a question of where a process lives rather
//!   than of what it is permitted to do.
//! - **The IPC endpoint is a same-user named pipe**, not a `0600` unix socket.
//!   The access-control mechanism is the pipe's ACL rather than file mode bits.
//! - **Auto-start is opt-in and separate.** `cua-driver autostart` is a
//!   Windows-only subcommand registering a logon Scheduled Task. It reports
//!   `not-registered` on a fresh install, so a Windows box has no daemon
//!   running until something starts one.
//!
//! The *conclusion* still holds: OxiMux does not own the daemon, so it must not
//! reap it. `cua-driver mcp` still starts one when none is up — the CLI
//! documents `--direct` as "own the runtime in this MCP process", which means
//! the default path does not, and describes the compat flag as forwarded "to
//! the proxy-launched daemon (… the path you actually run)".
//!
//! **Not confirmed by running it.** Settling the Windows posture empirically
//! means launching `cua-driver mcp`, which starts a process that can drive the
//! screen; that was left for a deliberate, sighted moment rather than done as a
//! side effect of a spike. Until then this section is read off the driver's own
//! CLI surface, and [`crate::session::reconcile`] must not be simplified on the
//! assumption that sessions die with the proxy.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::run_bounded;
use crate::Error;

/// Ceiling on a status probe. Generous relative to the sub-second normal case,
/// because a cold daemon launch is the slow path being measured.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// What the driver reports about its daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Running {
        socket: Option<PathBuf>,
        pid: Option<u32>,
    },
    /// Not up. Normal and self-healing — the `mcp` proxy starts it on demand.
    NotRunning,
    /// Reachable but reporting something unrecognised. Kept distinct from
    /// `NotRunning` so a settings pane can say "unexpected" rather than
    /// claiming a clean stopped state it did not observe.
    Unknown { detail: String },
}

impl DaemonState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Ask the driver whether its daemon is up.
pub fn status(driver: &Path, timeout: Duration) -> Result<DaemonState, Error> {
    let out = run_bounded(driver, &["status"], timeout)?;
    let text = if out.stdout.trim().is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    Ok(parse_status(&text))
}

/// Parse `cua-driver status` output.
///
/// This is human-readable text with no `--json` alternative in the driver's
/// CLI manifest, so it is parsed defensively: the running/not-running verdict
/// comes from a phrase match, and socket/pid are best-effort extras that a
/// wording change can drop without turning the verdict wrong. Order matters —
/// "is not running" contains "running".
fn parse_status(text: &str) -> DaemonState {
    let lower = text.to_ascii_lowercase();
    if lower.contains("is not running") {
        return DaemonState::NotRunning;
    }
    if lower.contains("is running") {
        return DaemonState::Running {
            socket: labelled(text, "socket:").map(PathBuf::from),
            pid: labelled(text, "pid:").and_then(|v| v.parse().ok()),
        };
    }
    DaemonState::Unknown {
        detail: text.trim().chars().take(200).collect(),
    }
}

/// Value following a `label:` on its own line.
fn labelled(text: &str, label: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(label)
            .map(|rest| rest.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim output from the installed driver.
    const RUNNING: &str = "Cua Driver daemon is running\n  \
socket: /Users/u/Library/Caches/cua-driver/cua-driver.sock\n  pid: 61106\n";
    const NOT_RUNNING: &str = "Cua Driver daemon is not running\n";

    /// Verbatim from `cua-driver 0.14.2` on Windows 10, captured 2026-08-01.
    ///
    /// Identical wording to [`NOT_RUNNING`], which is the useful fact: the
    /// phrase match ports unchanged. Kept as its own constant anyway, because
    /// "we checked and it is the same" and "nobody checked" look alike in a
    /// file that only has the macOS string.
    ///
    /// Two things that could have broken the caller and did not:
    ///
    /// - The verdict arrives on **stderr**, with stdout empty. [`status`]
    ///   already falls back to stderr when stdout is blank.
    /// - `status` exits **1** when the daemon is down. [`crate::exec::run_bounded`]
    ///   returns the code rather than erroring on it, and [`status`] ignores it.
    ///
    /// On an interactive console the driver also prints a telemetry notice
    /// above the verdict. It does not appear under the redirected pipes
    /// `run_bounded` always uses, and `contains` would tolerate it regardless.
    const NOT_RUNNING_WINDOWS: &str = "Cua Driver daemon is not running";

    #[test]
    fn parses_a_running_daemon_with_socket_and_pid() {
        assert_eq!(
            parse_status(RUNNING),
            DaemonState::Running {
                socket: Some(PathBuf::from(
                    "/Users/u/Library/Caches/cua-driver/cua-driver.sock"
                )),
                pid: Some(61106),
            }
        );
    }

    #[test]
    fn not_running_is_not_misread_as_running() {
        // "is not running" contains "is running"; a careless `contains` check
        // would report a stopped daemon as up.
        assert_eq!(parse_status(NOT_RUNNING), DaemonState::NotRunning);
        assert!(!parse_status(NOT_RUNNING).is_running());
    }

    #[test]
    fn a_running_daemon_without_details_is_still_running() {
        // The verdict must not depend on the optional extras.
        assert_eq!(
            parse_status("Cua Driver daemon is running\n"),
            DaemonState::Running {
                socket: None,
                pid: None
            }
        );
    }

    #[test]
    fn the_windows_wording_parses_the_same_way() {
        // Measured, not assumed. A reworded Windows `status` would have made
        // this parser return `Unknown` for every probe, and a settings pane
        // would have reported "unexpected" forever with nothing failing.
        assert_eq!(parse_status(NOT_RUNNING_WINDOWS), DaemonState::NotRunning);
        assert!(!parse_status(NOT_RUNNING_WINDOWS).is_running());
    }

    #[test]
    fn a_telemetry_banner_above_the_verdict_does_not_hide_it() {
        // What the driver prints on an interactive console. `run_bounded`
        // redirects, so this is defence rather than a live case — but the
        // verdict must survive anything printed around it.
        let noisy = format!(
            "Cua Driver sends content-free product telemetry by default. Run \
             `cua-driver telemetry disable` to stop it.\n{NOT_RUNNING_WINDOWS}\n"
        );
        assert_eq!(parse_status(&noisy), DaemonState::NotRunning);
    }

    #[test]
    fn unrecognised_output_is_unknown_not_stopped() {
        let state = parse_status("some future wording");
        assert!(matches!(state, DaemonState::Unknown { .. }));
        assert!(!state.is_running());
    }

    #[test]
    fn a_malformed_pid_does_not_invalidate_the_verdict() {
        let state = parse_status("Cua Driver daemon is running\n  pid: not-a-number\n");
        assert_eq!(
            state,
            DaemonState::Running {
                socket: None,
                pid: None
            }
        );
    }

    #[test]
    fn labelled_ignores_an_empty_value() {
        assert_eq!(labelled("pid:\n", "pid:"), None);
        assert_eq!(labelled("  pid: 12\n", "pid:").as_deref(), Some("12"));
    }
}
