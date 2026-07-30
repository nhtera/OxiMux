//! Supply-chain gate for the third-party driver binary.
//!
//! # Why signature verification rather than a pinned SHA-256
//!
//! The obvious integrity control for a third-party binary is a pinned hash.
//! That is the right answer when the host *downloads* the artifact — it is not
//! the right answer here, and shipping it would have been security theatre:
//!
//! - OxiMux never downloads the driver. The user installs it, and the driver
//!   ships its own `update --apply` that rewrites the binary in place.
//! - Upstream releases roughly daily. A pinned hash would go stale within days
//!   and turn every legitimate update into "computer use stopped working",
//!   whose only cure is bumping the constant — i.e. exactly the reflexive
//!   hash-bumping that makes a hash pin worthless.
//!
//! The binary is Developer-ID signed and notarized, so a stronger control is
//! available and survives updates:
//!
//! 1. `codesign --verify --strict` — the binary is intact. A tampered copy
//!    exits non-zero.
//! 2. `Identifier` + `TeamIdentifier` — it came from *this* publisher. An
//!    unrelated (even Apple-signed) binary passes step 1 but reports a
//!    different identifier and `TeamIdentifier=not set`, so step 1 alone is
//!    not a gate.
//! 3. Notarization ticket stapled — Apple has seen it.
//!
//! The gates themselves live in `oximux-macos-trust` (shared with the app's
//! own updater — forked copies of signature-checking code would drift); this
//! module owns the driver-specific policy, the version floor, and the audit
//! hash.
//!
//! The observed SHA-256 is still computed and reported, so the settings pane
//! and bug reports can state exactly which bytes ran. It is an audit trail,
//! not a gate — the distinction the `dictation` model catalog got wrong when
//! it shipped `archive_sha256: None` and then skipped verification entirely.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_macos_trust::{SignaturePolicy, TrustError};
use sha2::{Digest, Sha256};

use crate::exec::run_bounded;
use crate::version::Version;
use crate::Error;

/// Code-signing identifier the driver must present.
pub const EXPECTED_IDENTIFIER: &str = "com.trycua.driver";

/// Apple Developer Team ID the driver must be signed by. This is the pin that
/// makes a substituted binary fail: anyone can produce a validly signed
/// binary, but not one signed by this team.
pub const EXPECTED_TEAM_ID: &str = "YCK386LBJ7";

/// Oldest driver whose CLI surface this integration was built against.
/// `--host-bundle-id` and the machine-readable `manifest` verb both need it.
pub const MIN_VERSION: Version = Version::new(0, 12, 6);

/// Ceiling for the driver's own `--version` run — the driver talks to a
/// machine-wide daemon and a wedged one must not freeze the caller.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// A driver that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDriver {
    pub path: PathBuf,
    pub version: Version,
    pub identifier: String,
    pub team_id: String,
    pub notarized: bool,
    /// Audit trail, not a gate — see the module docs.
    pub sha256: String,
}

fn driver_policy() -> SignaturePolicy {
    SignaturePolicy {
        identifier: EXPECTED_IDENTIFIER.to_string(),
        team_id: EXPECTED_TEAM_ID.to_string(),
    }
}

/// Run every gate against `path`, or fail with the specific reason.
pub fn verify(path: &Path) -> Result<VerifiedDriver, Error> {
    let signature =
        oximux_macos_trust::verify_signed(path, &driver_policy()).map_err(from_trust)?;

    let version = read_version(path)?;
    if version < MIN_VERSION {
        return Err(Error::DriverTooOld {
            found: version,
            minimum: MIN_VERSION,
        });
    }

    Ok(VerifiedDriver {
        path: path.to_path_buf(),
        version,
        identifier: signature.identifier,
        team_id: signature.team_id,
        notarized: signature.notarized,
        sha256: sha256_of(path)?,
    })
}

/// Gatekeeper's own verdict on an app bundle — the notarization gate the
/// in-app installer needs, since its programmatic download is never
/// quarantined. See `oximux_macos_trust::verify_notarized_bundle`.
pub fn verify_notarized_bundle(bundle: &Path) -> Result<(), Error> {
    oximux_macos_trust::verify_notarized_bundle(bundle).map_err(from_trust)
}

/// The code-signing identifier of any binary — for an app bundle, its
/// `CFBundleIdentifier`. `None` when the binary is unsigned, missing, or
/// otherwise unreadable.
///
/// Distinct from [`verify`]: that one gates the *driver* and fails loudly.
/// This one just reads an identity off an arbitrary program, for callers that
/// treat "unknown" as an ordinary answer rather than an error.
pub fn signing_identifier(path: &Path) -> Option<String> {
    oximux_macos_trust::read_signature(path)
        .ok()
        .map(|signature| signature.identifier)
        .filter(|identifier| !identifier.is_empty())
}

/// Map the shared gate's errors onto this crate's user-facing variants. The
/// `expected` sides come from the local constants so settings-pane messages
/// keep naming the driver's real policy.
pub(crate) fn from_trust(err: TrustError) -> Error {
    match err {
        TrustError::Spawn { program, source } => Error::Spawn { program, source },
        TrustError::Timeout { program, timeout } => Error::Timeout { program, timeout },
        TrustError::SignatureInvalid { path, detail } => Error::SignatureInvalid { path, detail },
        TrustError::UnexpectedIdentifier { found, .. } => Error::UnexpectedIdentifier {
            found,
            expected: EXPECTED_IDENTIFIER,
        },
        TrustError::UnexpectedTeamId { found, .. } => Error::UnexpectedTeamId {
            found,
            expected: EXPECTED_TEAM_ID,
        },
        TrustError::NotNotarized { path, detail } => Error::NotNotarized { path, detail },
        // `ditto` never runs on the verify paths this module wraps.
        TrustError::DittoFailed { detail } => Error::SessionCommandFailed {
            command: "ditto",
            detail,
        },
    }
}

/// `cua-driver --version` prints `cua-driver 0.12.6`.
fn read_version(path: &Path) -> Result<Version, Error> {
    let out = run_bounded(path, &["--version"], VERSION_TIMEOUT)?;
    let raw = if out.stdout.trim().is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    Version::parse_from_output(&raw).ok_or_else(|| Error::UnreadableVersion {
        output: raw.chars().take(200).collect(),
    })
}

fn sha256_of(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Spawn {
        program: path.display().to_string(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_errors_keep_the_driver_policy_in_the_message() {
        // The user needs the message to name the real pin, not a generic one.
        let err = from_trust(TrustError::UnexpectedTeamId {
            found: "not set".into(),
            expected: "ignored".into(),
        });
        let msg = err.to_string();
        assert!(msg.contains("not set"), "{msg}");
        assert!(msg.contains(EXPECTED_TEAM_ID), "{msg}");
    }

    /// The install-path gate must *reject*, not merely report — and the
    /// rejection must surface as this crate's own `NotNotarized` so the
    /// settings pane keeps its specific messaging.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unnotarized_bundle_is_rejected_not_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("Fake.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).expect("mkdir");
        std::fs::copy("/bin/ls", bundle.join("Contents/MacOS/Fake")).expect("copy");
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            "<plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>test.fake</string>\
             <key>CFBundleExecutable</key><string>Fake</string>\
             </dict></plist>",
        )
        .expect("plist");

        let err = verify_notarized_bundle(&bundle).expect_err("must reject");
        assert!(matches!(err, Error::NotNotarized { .. }), "got {err:?}");
    }
}
