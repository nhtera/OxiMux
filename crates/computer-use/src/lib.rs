//! Screen control for agents, backed by an external `cua-driver` daemon.
//!
//! # What this crate owns — and what it deliberately does not
//!
//! The driver is a notarized third-party binary the *user* installs. It runs a
//! machine-wide daemon shared with every other MCP client, and it starts that
//! daemon on demand by itself. So this crate:
//!
//! - **verifies** the binary before OxiMux will declare it to any agent,
//! - **declares** it as an MCP server ([`mcp::server_spec`]),
//! - **reads** daemon state ([`daemon::status`]) without managing it,
//! - **cleans up its own sessions** ([`session::reconcile`]).
//!
//! It does not start, supervise, or reap the daemon. See [`daemon`] for the
//! evidence behind that, which contradicted the original design.
//!
//! Everything here is plain synchronous Rust with no GPUI or async runtime, so
//! it stays unit-testable — but it is intentionally *callable* from the UI
//! thread, because the permission handler that will gate these tools is
//! synchronous. That is what the bounded timeouts in [`exec`] are protecting.

pub mod active;
pub mod blocked;
pub mod daemon;
pub mod discovery;
pub mod exec;
pub mod grants;
pub mod gui_scripting;
pub mod lifecycle;
pub mod mcp;
pub mod policy;
pub mod proc;
pub mod redact;
pub mod session;
pub mod target;
pub mod tools;
pub mod verify;
pub mod version;

use std::path::PathBuf;
use std::time::Duration;

/// OxiMux's own bundle identifier.
///
/// Used two ways that must not drift apart: as the driver's advisory host label
/// (so `check_permissions` names who asked), and as the identity an agent is
/// never allowed to drive. Must track `CFBundleIdentifier` in
/// `assets/Info.plist`.
pub const HOST_BUNDLE_ID: &str = "dev.nhtera.oximux";

/// The enforcing hook's binary, which ships beside the app executable.
///
/// Named here rather than at the one place that looks for it because the bundle
/// script copies it in under this name and `[[bin]]` builds it under this name:
/// three spellings of one string, and the two that are not Rust cannot be
/// checked by anything. Keep them in step with
/// `scripts/bundle-macos.sh`.
pub const GATE_BINARY_NAME: &str = "oximux-screen-gate";

pub use active::{Driving, DrivingSession};
pub use daemon::DaemonState;
pub use grants::{GrantTable, Provenance};
pub use gui_scripting::GuiScripting;
pub use mcp::{bare_tool_name, is_computer_use_tool, server_spec, SERVER_NAME};
pub use policy::{decide, Decision, PolicyContext};
pub use redact::{scrub_transcript, ScreenshotFilter};
pub use session::{Reconciliation, SessionId, SessionLedger};
pub use target::{Category, TargetApp};
pub use verify::VerifiedDriver;
pub use version::Version;

/// Everything that can stop OxiMux from offering screen control.
///
/// Each variant names a distinct cause because these surface to the user in
/// settings: "not installed" and "signed by someone else" call for very
/// different responses, and collapsing them into one string would hide the
/// only one that matters.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no cua-driver found (looked in: {})", searched.join(", "))]
    NotFound { searched: Vec<String> },

    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },

    #[error("{program} did not respond within {timeout:?}")]
    Timeout { program: String, timeout: Duration },

    #[error("signature check failed for {}: {detail}", path.display())]
    SignatureInvalid { path: PathBuf, detail: String },

    #[error("driver identifies as `{found}`, expected `{expected}`")]
    UnexpectedIdentifier {
        found: String,
        expected: &'static str,
    },

    #[error("driver is signed by team `{found}`, expected `{expected}`")]
    UnexpectedTeamId {
        found: String,
        expected: &'static str,
    },

    #[error("driver {found} is older than the required {minimum}")]
    DriverTooOld { found: Version, minimum: Version },

    #[error("could not read a version from the driver (got: {output})")]
    UnreadableVersion { output: String },

    #[error("{command} failed: {detail}")]
    SessionCommandFailed {
        command: &'static str,
        detail: String,
    },
}

/// Find the driver and put it through every gate — equivalently, "is screen
/// control available, and if not, why not?"
///
/// The single entry point callers should use: nothing else in this crate
/// should be reached with an unverified path, because handing an unverified
/// binary to [`mcp::server_spec`] would declare it to an agent.
///
/// Cheap enough for a settings pane to call on open — the expensive part is
/// `codesign`, which is sub-second.
pub fn prepare() -> Result<VerifiedDriver, Error> {
    let path = discovery::locate()?;
    verify::verify(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_error_lists_what_was_searched() {
        let err = Error::NotFound {
            searched: vec!["/Applications/…".into(), "$PATH".into()],
        };
        let msg = err.to_string();
        assert!(msg.contains("/Applications/…"), "{msg}");
        assert!(msg.contains("$PATH"), "{msg}");
    }

    #[test]
    fn identity_errors_name_both_sides() {
        // The user needs to know what was found, not just that it was wrong.
        let err = Error::UnexpectedTeamId {
            found: "not set".into(),
            expected: verify::EXPECTED_TEAM_ID,
        };
        let msg = err.to_string();
        assert!(msg.contains("not set"), "{msg}");
        assert!(msg.contains(verify::EXPECTED_TEAM_ID), "{msg}");
    }

    #[test]
    fn too_old_error_names_the_required_floor() {
        let err = Error::DriverTooOld {
            found: Version::new(0, 11, 0),
            minimum: verify::MIN_VERSION,
        };
        let msg = err.to_string();
        assert!(msg.contains("0.11.0"), "{msg}");
        assert!(msg.contains(&verify::MIN_VERSION.to_string()), "{msg}");
    }

    #[test]
    fn locating_an_overridden_missing_driver_fails_rather_than_falling_back() {
        // A developer pointing at a local build must not silently get the
        // installed one instead.
        temp_env_var(discovery::PATH_OVERRIDE_ENV, "/nonexistent/cua-driver", || {
            let err = discovery::locate().expect_err("must fail");
            assert!(matches!(err, Error::NotFound { .. }), "got {err:?}");
        });
    }

    /// Set an env var for the duration of `body`. Tests touching process env
    /// are kept to one place and run serially within this module.
    fn temp_env_var(key: &str, value: &str, body: impl FnOnce()) {
        let previous = std::env::var_os(key);
        // SAFETY: single-threaded within this test; no other thread reads env.
        unsafe { std::env::set_var(key, value) };
        body();
        match previous {
            Some(old) => unsafe { std::env::set_var(key, old) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
