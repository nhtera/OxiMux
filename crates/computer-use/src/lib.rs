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
//!
//! # Windows: the gate, and why it is open
//!
//! [`mcp::server_spec`] does not exist on Windows unless the
//! `windows-screen-control` feature is on. The desktop app now enables it, so a
//! Windows build declares the driver like any other — but the feature is kept
//! rather than deleted, because the crate's own tests still build the gated-off
//! shape and because turning it on is a decision about handing agents an
//! unsigned third-party binary. That belongs in a manifest, not buried in a
//! `cfg`. [`declares_driver`] reports which side of it a build is on.
//!
//! The plan this came from called for a crate-wide `compile_error!` instead.
//! That was rejected on contact: it would have made the crate unbuildable on
//! Windows, which also makes it untestable — and the same plan requires the
//! platform-independent modules' tests to pass there. A guard that has to be
//! switched off to run the tests it guards is not a guard.
//!
//! ## Four reasons it stayed shut, and what discharged each
//!
//! Worth keeping, because each was the *whole* answer at the time and three of
//! them turned out to be a layer above the real one.
//!
//! 1. **"No Windows equivalent exists."** Out of date — `cua-driver` has shipped
//!    Windows binaries since v0.1.3.
//! 2. **The policing hook must ship with any screen-driving capability**
//!    (`docs/windows-port-exclusions.md`), since "nothing in the compiler would
//!    say so". Met: [`gui_scripting`] classifies the Windows reach and
//!    `platform::escape_tap` swallows Escape through a `WH_KEYBOARD_LL` hook.
//!    That condition had in fact been met *before* the driver arrived, because
//!    input synthesis on Windows needs no permission at all.
//! 3. **No trust anchor.** [`verify`]'s model is Apple code signing and the
//!    Windows artifacts are unsigned. Answered by [`trust`]: the user approves
//!    the exact bytes and OxiMux refuses if they change. A weaker anchor than a
//!    signature, deliberately — it establishes **continuity, not identity**.
//!    Read that module before describing it to a user as verification.
//! 4. **No way to say yes.** An anchor whose approval step has no UI refuses
//!    everything for ever. `settings_modal::pane_driver_trust` is that surface.
//!
//! ## What Windows still does not get
//!
//! Two, both recorded in `docs/windows-port-exclusions.md` rather than hidden:
//!
//! - **No in-app installer.** [`install`] downloads and stages a notarized
//!   `.app`; none of that has a Windows meaning. The user installs the driver
//!   themselves, which is also what makes the trust anchor coherent.
//! - **A weaker "an agent is driving" indicator.** The macOS menu-bar item is
//!   visible the moment it is created; a Windows notification-area icon can sit
//!   in the overflow flyout until the user pins it.

pub mod active;
pub mod blocked;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod daemon;
pub mod discovery;
pub mod exec;
pub mod grants;
pub mod gui_scripting;
pub mod install;
pub mod lifecycle;
pub mod mcp;
pub mod policy;
pub mod proc;
pub mod session;
pub mod target;
pub mod tools;
pub mod trust;
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

/// [`GATE_BINARY_NAME`] with the platform's executable extension.
///
/// The constant is the `[[bin]]` name and the name the bundle script copies in
/// under; this is what is actually on disk. They differ only on Windows, where
/// the file is `oximux-screen-gate.exe` — and a lookup for the extensionless
/// name there finds nothing, which reads as "no gate installed" and silently
/// runs chats unenforced.
///
/// `EXE_SUFFIX` is empty on macOS, so callers use this unconditionally rather
/// than branching.
pub fn gate_binary_file_name() -> String {
    format!("{GATE_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX)
}

/// Does this build hand the driver to agents at all?
///
/// False only on a Windows build without `windows-screen-control`, where
/// [`mcp::server_spec`] does not exist and [`mcp::declaration`] returns the hook
/// alone. Exposed as a function because a dependent crate cannot ask about this
/// crate's features with `cfg!`, and one surface genuinely needs to: the Windows
/// settings pane collects an approval for a driver that nothing yet declares,
/// and its disclosure of that has to stop being true the moment this flips —
/// otherwise the pane keeps apologising for a limitation that was lifted.
pub const fn declares_driver() -> bool {
    cfg!(any(not(windows), feature = "windows-screen-control"))
}

pub use active::{Driving, DrivingSession};
pub use daemon::DaemonState;
pub use grants::{GrantTable, Provenance};
pub use gui_scripting::GuiScripting;
pub use mcp::{bare_tool_name, is_computer_use_tool, SERVER_NAME};
// Absent on Windows until the safety pair lands — see [`mcp::server_spec`].
#[cfg(any(not(windows), feature = "windows-screen-control"))]
pub use mcp::server_spec;
pub use policy::{decide, Decision, PolicyContext};
// Screenshot scrubbing moved to `oximux-agent-core` so it keeps compiling on
// platforms this crate does not build for (see that crate's `redact` module).
// Re-exported under the old path because this crate is where callers expect
// anything screen-control-shaped to live.
pub use oximux_agent_core::redact::{scrub_transcript, ScreenshotFilter};
pub use session::{Reconciliation, SessionId, SessionLedger};
pub use target::{Category, TargetApp};
pub use trust::{Trust, TrustStore};
pub use verify::{TrustBasis, VerifiedDriver};
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

    #[error(
        "{} failed Gatekeeper assessment (not notarized?): {detail}",
        path.display()
    )]
    NotNotarized { path: PathBuf, detail: String },

    #[error("could not read a version from the driver (got: {output})")]
    UnreadableVersion { output: String },

    #[error("{command} failed: {detail}")]
    SessionCommandFailed {
        command: &'static str,
        detail: String,
    },

    /// The user has never approved this binary. Not a failure — the normal
    /// first-run state everywhere the trust anchor is the user rather than a
    /// publisher. Carries the digest so the prompt can show what it is asking
    /// about without re-reading the file.
    #[error("{} has not been approved (sha256 {sha256})", path.display())]
    NotApproved { path: PathBuf, sha256: String },

    /// Approved once, and the bytes changed. The case [`trust`] exists to catch,
    /// and kept separate from [`Error::NotApproved`] because it is the only one
    /// of the two that should alarm anyone.
    #[error(
        "{} changed since it was approved (approved {approved}, found {found})",
        path.display()
    )]
    TrustSuperseded {
        path: PathBuf,
        approved: String,
        found: String,
    },

    /// The pin store itself could not be read or written. Distinct from "nothing
    /// approved": a corrupt store reads as empty and asks again, but an
    /// unwritable one cannot record an approval at all, so the user would be
    /// asked forever with no way to answer.
    #[error("trust store at {} is unusable: {source}", path.display())]
    TrustStoreUnusable {
        path: PathBuf,
        source: std::io::Error,
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
#[cfg(not(windows))]
pub fn prepare() -> Result<VerifiedDriver, Error> {
    let path = discovery::locate()?;
    verify::verify(&path)
}

/// The Windows entry point, which takes the trust anchor as an argument because
/// Windows has no ambient one.
///
/// The differing signature is the point rather than an inconvenience: there are
/// no unsigned Windows binaries this crate will vouch for on its own, so the
/// type system requires every Windows caller to name whose approval it is
/// relying on. A no-argument `prepare()` on Windows would have to invent an
/// answer, and the only answers available are "trust anything" or a hardcoded
/// path this crate has no business resolving.
///
/// Expect [`Error::NotApproved`] on first run. That is the normal state, not a
/// failure — the caller's job is to prompt, call [`trust::TrustStore::approve`],
/// and try again.
#[cfg(windows)]
pub fn prepare(trust: &trust::TrustStore) -> Result<VerifiedDriver, Error> {
    let path = discovery::locate()?;
    verify::verify_pinned(&path, trust)
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
