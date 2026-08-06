//! The three gates every update passes, in the order they must run.
//!
//! 1. **Signature** over the manifest bytes, against the key compiled into
//!    this binary. This is the trust root and the only gate that survives a
//!    compromised publish token: the manifest and the artifacts come from the
//!    same GitHub Release, so a sha256 taken from the manifest proves only
//!    that the download matches what the publisher said.
//! 2. **Digest** of the downloaded archive against the (now trusted) manifest.
//! 3. **Monotonicity** — the offered version must be strictly greater than the
//!    running one, so a validly signed *old* release cannot be replayed to
//!    walk an installation backwards onto a fixed bug.
//!
//! Nothing here reaches the network or the filesystem, which is what makes all
//! three testable against hand-made tampering.

use oximux_auto_update::Version;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// This build has no release key, so it cannot verify anything. Refusing
    /// is the only safe answer — falling back to the manifest's own checksum
    /// would be trusting the artifact to vouch for itself.
    NoTrustRoot,
    MalformedKey,
    MalformedSignature(String),
    /// The signature did not verify. Deliberately carries no detail beyond
    /// this: a tampered manifest and a stale one are the same answer, and the
    /// bytes that failed must not be echoed anywhere.
    BadSignature,
    DigestMismatch { expected: String, actual: String },
    UnreadableVersion { raw: String },
    NotAnUpgrade { offered: String, running: String },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTrustRoot => write!(
                f,
                "this build has no release signing key compiled in, so it cannot verify an \
                 update — install oximux from a release build or your package manager"
            ),
            Self::MalformedKey => write!(
                f,
                "the release signing key compiled into this build is not a valid minisign key"
            ),
            Self::MalformedSignature(detail) => {
                write!(f, "the release signature could not be read: {detail}")
            }
            Self::BadSignature => write!(
                f,
                "the release manifest is not signed by the OxiMux release key — refusing to \
                 update. This is what a tampered or substituted release looks like."
            ),
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "the downloaded archive does not match the signed manifest \
                 (expected sha256 {expected}, got {actual})"
            ),
            Self::UnreadableVersion { raw } => {
                write!(f, "cannot read {raw:?} as a version number")
            }
            Self::NotAnUpgrade { offered, running } => write!(
                f,
                "the release offers {offered} and this is {running} — refusing to move backwards"
            ),
        }
    }
}

/// Gate 1. `public_key` is the base64 body of a minisign `.pub`; `signature`
/// is the whole `.minisig` file.
///
/// Legacy (non-prehashed) minisign signatures are **not** accepted. Current
/// minisign and rsign2 both produce prehashed signatures by default, so
/// allowing the legacy mode would only widen what this accepts without
/// widening what the release workflow can produce.
pub fn verify_manifest_signature(
    manifest: &[u8],
    signature: &str,
    public_key: Option<&str>,
) -> Result<(), VerifyError> {
    let key = public_key.ok_or(VerifyError::NoTrustRoot)?;
    let key = minisign_verify::PublicKey::from_base64(key.trim())
        .map_err(|_| VerifyError::MalformedKey)?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|err| VerifyError::MalformedSignature(err.to_string()))?;
    key.verify(manifest, &signature, false).map_err(|_| VerifyError::BadSignature)
}

/// Gate 2. Compares case-insensitively — the hex case in a manifest is a
/// formatting choice, not part of the digest.
pub fn verify_digest(bytes: &[u8], expected: &str) -> Result<(), VerifyError> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected.trim()) {
        return Ok(());
    }
    Err(VerifyError::DigestMismatch { expected: expected.trim().to_string(), actual })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Gate 3. Ported from the desktop updater's rule
/// (`crates/auto-update/src/pipeline.rs`), sharing its `Version` type so the
/// two cannot drift on what counts as an upgrade.
pub fn verify_is_upgrade(offered: &str, running: &str) -> Result<Version, VerifyError> {
    let offered_v = Version::parse(offered)
        .ok_or_else(|| VerifyError::UnreadableVersion { raw: offered.to_string() })?;
    let running_v = Version::parse(running)
        .ok_or_else(|| VerifyError::UnreadableVersion { raw: running.to_string() })?;
    if offered_v > running_v {
        Ok(offered_v)
    } else {
        Err(VerifyError::NotAnUpgrade {
            offered: offered_v.to_string(),
            running: running_v.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testkit::MinisignKeypair;

    #[test]
    fn a_signature_from_the_release_key_verifies() {
        let keys = MinisignKeypair::generate();
        let manifest = br#"{"schemaVersion":1}"#;
        let sig = keys.sign(manifest);
        assert_eq!(
            verify_manifest_signature(manifest, &sig, Some(&keys.public_key_base64())),
            Ok(())
        );
    }

    /// The case checksums alone miss, and the reason the manifest is signed at
    /// all: the bytes hash to exactly what the (tampered) manifest claims,
    /// because the attacker rewrote both.
    #[test]
    fn a_tampered_manifest_with_a_consistent_checksum_is_still_refused() {
        let keys = MinisignKeypair::generate();
        let honest = br#"{"schemaVersion":1,"version":"0.2.0"}"#;
        let sig = keys.sign(honest);

        let tampered = br#"{"schemaVersion":1,"version":"6.6.6"}"#;
        // Internally consistent: the tampered bytes hash to their own digest.
        assert_eq!(verify_digest(tampered, &sha256_hex(tampered)), Ok(()));
        // And still refused, because the signature covers the honest bytes.
        assert_eq!(
            verify_manifest_signature(tampered, &sig, Some(&keys.public_key_base64())),
            Err(VerifyError::BadSignature)
        );
    }

    /// A signature that is perfectly valid — under somebody else's key.
    #[test]
    fn a_signature_from_a_different_key_is_refused() {
        let ours = MinisignKeypair::generate();
        let theirs = MinisignKeypair::generate();
        let manifest = br#"{"schemaVersion":1}"#;
        let sig = theirs.sign(manifest);
        assert_eq!(
            verify_manifest_signature(manifest, &sig, Some(&ours.public_key_base64())),
            Err(VerifyError::BadSignature)
        );
    }

    /// minisign's trusted comment is covered by a second, global signature.
    /// Rewriting it must not survive — otherwise "trusted comment" is a lie.
    #[test]
    fn rewriting_the_trusted_comment_breaks_verification() {
        let keys = MinisignKeypair::generate();
        let manifest = br#"{"schemaVersion":1}"#;
        let sig = keys.sign(manifest);
        let forged = sig.replace("timestamp:1", "timestamp:9");
        assert_ne!(forged, sig, "the fixture must actually carry that comment");
        assert!(matches!(
            verify_manifest_signature(manifest, &forged, Some(&keys.public_key_base64())),
            Err(VerifyError::BadSignature | VerifyError::MalformedSignature(_))
        ));
    }

    /// A build with no key must refuse, not fall through to checksum-only.
    #[test]
    fn no_compiled_in_key_means_no_update_at_all() {
        let keys = MinisignKeypair::generate();
        let manifest = br#"{"schemaVersion":1}"#;
        let sig = keys.sign(manifest);
        assert_eq!(
            verify_manifest_signature(manifest, &sig, None),
            Err(VerifyError::NoTrustRoot)
        );
    }

    #[test]
    fn a_digest_mismatch_names_both_sides() {
        let err = verify_digest(b"actual bytes", &"a".repeat(64)).expect_err("mismatch");
        let rendered = err.to_string();
        assert!(rendered.contains(&sha256_hex(b"actual bytes")), "{rendered}");
        assert!(rendered.contains(&"a".repeat(64)), "{rendered}");
    }

    #[test]
    fn digest_comparison_ignores_hex_case() {
        let upper = sha256_hex(b"x").to_uppercase();
        assert_eq!(verify_digest(b"x", &upper), Ok(()));
    }

    /// The downgrade gate. Equal is not an upgrade either — re-running the
    /// same version would swap binaries for nothing.
    #[test]
    fn only_a_strictly_greater_version_is_an_upgrade() {
        assert!(verify_is_upgrade("0.2.0", "0.1.6").is_ok());
        assert!(verify_is_upgrade("0.10.0", "0.9.9").is_ok(), "numeric, not lexicographic");
        for (offered, running) in [("0.1.5", "0.1.6"), ("0.1.6", "0.1.6"), ("0.9.9", "0.10.0")] {
            assert_eq!(
                verify_is_upgrade(offered, running),
                Err(VerifyError::NotAnUpgrade {
                    offered: offered.into(),
                    running: running.into()
                }),
                "{offered} over {running}"
            );
        }
    }

    #[test]
    fn an_unparseable_version_is_refused_rather_than_treated_as_zero() {
        assert!(matches!(
            verify_is_upgrade("not-a-version", "0.1.6"),
            Err(VerifyError::UnreadableVersion { .. })
        ));
    }
}
