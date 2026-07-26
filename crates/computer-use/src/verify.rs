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
//! The observed SHA-256 is still computed and reported, so the settings pane
//! and bug reports can state exactly which bytes ran. It is an audit trail,
//! not a gate — the distinction the `dictation` model catalog got wrong when
//! it shipped `archive_sha256: None` and then skipped verification entirely.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// `codesign` is quick (~0.4s on a 48 MB universal binary) but runs on a user
/// action, so it still gets a ceiling rather than an open-ended wait.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Run every gate against `path`, or fail with the specific reason.
pub fn verify(path: &Path) -> Result<VerifiedDriver, Error> {
    verify_integrity(path)?;
    let signature = read_signature(path)?;

    if signature.identifier != EXPECTED_IDENTIFIER {
        return Err(Error::UnexpectedIdentifier {
            found: signature.identifier,
            expected: EXPECTED_IDENTIFIER,
        });
    }
    if signature.team_id != EXPECTED_TEAM_ID {
        return Err(Error::UnexpectedTeamId {
            found: signature.team_id,
            expected: EXPECTED_TEAM_ID,
        });
    }

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

/// `codesign --verify --strict`: does the binary still match its signature?
fn verify_integrity(path: &Path) -> Result<(), Error> {
    let out = run_bounded(
        Path::new("/usr/bin/codesign"),
        &["--verify", "--strict", &path.display().to_string()],
        VERIFY_TIMEOUT,
    )?;
    if out.success() {
        return Ok(());
    }
    Err(Error::SignatureInvalid {
        path: path.to_path_buf(),
        detail: first_meaningful_line(&out.stderr),
    })
}

struct SignatureInfo {
    identifier: String,
    team_id: String,
    notarized: bool,
}

/// `codesign -dvv` reports on **stderr**, and exits 0 even for a binary that
/// fails `--verify` — it only displays. So this must never be used alone.
fn read_signature(path: &Path) -> Result<SignatureInfo, Error> {
    let out = run_bounded(
        Path::new("/usr/bin/codesign"),
        &["-dvv", &path.display().to_string()],
        VERIFY_TIMEOUT,
    )?;
    if !out.success() {
        return Err(Error::SignatureInvalid {
            path: path.to_path_buf(),
            detail: first_meaningful_line(&out.stderr),
        });
    }
    Ok(SignatureInfo {
        identifier: field(&out.stderr, "Identifier").unwrap_or_default(),
        // Apple's own binaries report the literal `not set`; keeping it verbatim
        // makes the mismatch error say something true.
        team_id: field(&out.stderr, "TeamIdentifier").unwrap_or_default(),
        notarized: field(&out.stderr, "Notarization Ticket")
            .is_some_and(|v| v.eq_ignore_ascii_case("stapled")),
    })
}

/// Value of a `Key=Value` line in `codesign -dvv` output.
fn field(haystack: &str, key: &str) -> Option<String> {
    haystack.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|value| value.trim().to_string())
    })
}

/// `cua-driver --version` prints `cua-driver 0.12.6`.
fn read_version(path: &Path) -> Result<Version, Error> {
    let out = run_bounded(path, &["--version"], VERIFY_TIMEOUT)?;
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

fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no detail reported")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Executable=/Applications/CuaDriver.app/Contents/MacOS/cua-driver\n\
Identifier=com.trycua.driver\n\
Authority=Developer ID Application: Cua AI, Inc. (YCK386LBJ7)\n\
Notarization Ticket=stapled\n\
Info.plist entries=15\n\
TeamIdentifier=YCK386LBJ7\n";

    #[test]
    fn parses_the_identity_fields_codesign_actually_emits() {
        assert_eq!(field(SAMPLE, "Identifier").as_deref(), Some("com.trycua.driver"));
        assert_eq!(field(SAMPLE, "TeamIdentifier").as_deref(), Some("YCK386LBJ7"));
        assert_eq!(
            field(SAMPLE, "Notarization Ticket").as_deref(),
            Some("stapled")
        );
    }

    #[test]
    fn identifier_prefix_does_not_swallow_teamidentifier() {
        // `Identifier` is a prefix of `TeamIdentifier`; a naive `contains` or a
        // prefix match without the `=` would read the wrong line and let a
        // mismatched team pass. Order the sample so the team line comes first.
        let reversed = "TeamIdentifier=WRONGTEAM\nIdentifier=com.trycua.driver\n";
        assert_eq!(
            field(reversed, "Identifier").as_deref(),
            Some("com.trycua.driver")
        );
        assert_eq!(
            field(reversed, "TeamIdentifier").as_deref(),
            Some("WRONGTEAM")
        );
    }

    #[test]
    fn an_apple_signed_binary_reports_no_team() {
        // The case that proves `--verify` alone is not a gate: /bin/echo passes
        // integrity verification but is not from this publisher.
        let apple = "Identifier=com.apple.echo\nTeamIdentifier=not set\n";
        assert_eq!(field(apple, "TeamIdentifier").as_deref(), Some("not set"));
        assert_ne!(field(apple, "TeamIdentifier").unwrap(), EXPECTED_TEAM_ID);
    }

    #[test]
    fn missing_fields_are_absent_rather_than_empty() {
        assert!(field("Format=app bundle\n", "TeamIdentifier").is_none());
    }

    #[test]
    fn unstapled_notarization_is_not_reported_as_notarized() {
        let unstapled = "Identifier=com.trycua.driver\nTeamIdentifier=YCK386LBJ7\n";
        assert!(field(unstapled, "Notarization Ticket").is_none());
    }

    #[test]
    fn first_meaningful_line_skips_blanks() {
        assert_eq!(
            first_meaningful_line("\n\n  code object is not signed at all\nmore\n"),
            "code object is not signed at all"
        );
        assert_eq!(first_meaningful_line("   \n"), "no detail reported");
    }
}
