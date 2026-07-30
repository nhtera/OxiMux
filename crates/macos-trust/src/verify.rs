//! Code-signature gates: integrity, publisher identity, notarization.

use std::path::Path;
use std::time::Duration;

use crate::exec::run_bounded;
use crate::{SignaturePolicy, TrustError};

/// `codesign` is quick (~0.4s on a 48 MB universal binary) but runs on a user
/// action, so it still gets a ceiling rather than an open-ended wait.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// A binary's signing identity, as `codesign -dvv` reports it — verbatim.
///
/// `team_id` keeps whatever codesign printed, including the literal `not set`
/// Apple's own binaries report: some callers treat "unknown publisher" as an
/// ordinary answer, and rewriting the value would make their mismatch errors
/// lie. Callers that need a trust *anchor* must check [`Signature::pinnable`]
/// instead of mere non-emptiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub identifier: String,
    pub team_id: String,
    pub notarized: bool,
}

impl Signature {
    /// Whether this identity is strong enough to pin future verifications to.
    ///
    /// Ad-hoc and Apple-platform signatures report `TeamIdentifier=not set` —
    /// a value any local attacker can reproduce with `codesign -s -`. Pinning
    /// to it would make the pin satisfiable by anyone, so it is rejected here
    /// even though the lenient readers above pass it through.
    pub fn pinnable(&self) -> bool {
        !self.identifier.is_empty() && !self.team_id.is_empty() && self.team_id != "not set"
    }
}

/// All identity gates against `path`: integrity, then identifier + team
/// against `policy`. Returns the observed signature on success.
pub fn verify_signed(path: &Path, policy: &SignaturePolicy) -> Result<Signature, TrustError> {
    verify_integrity(path)?;
    let signature = read_signature(path)?;

    if signature.identifier != policy.identifier {
        return Err(TrustError::UnexpectedIdentifier {
            found: signature.identifier,
            expected: policy.identifier.clone(),
        });
    }
    if signature.team_id != policy.team_id {
        return Err(TrustError::UnexpectedTeamId {
            found: signature.team_id,
            expected: policy.team_id.clone(),
        });
    }
    Ok(signature)
}

/// `codesign --verify --strict`: does the binary still match its signature?
pub fn verify_integrity(path: &Path) -> Result<(), TrustError> {
    let out = run_bounded(
        Path::new("/usr/bin/codesign"),
        &["--verify", "--strict", &path.display().to_string()],
        VERIFY_TIMEOUT,
    )?;
    if out.success() {
        return Ok(());
    }
    Err(TrustError::SignatureInvalid {
        path: path.to_path_buf(),
        detail: first_meaningful_line(&out.stderr),
    })
}

/// `codesign -dvv` reports on **stderr**, and exits 0 even for a binary that
/// fails `--verify` — it only displays. So this must never be used alone as a
/// gate; [`verify_signed`] pairs it with [`verify_integrity`].
pub fn read_signature(path: &Path) -> Result<Signature, TrustError> {
    let out = run_bounded(
        Path::new("/usr/bin/codesign"),
        &["-dvv", &path.display().to_string()],
        VERIFY_TIMEOUT,
    )?;
    if !out.success() {
        return Err(TrustError::SignatureInvalid {
            path: path.to_path_buf(),
            detail: first_meaningful_line(&out.stderr),
        });
    }
    Ok(Signature {
        identifier: field(&out.stderr, "Identifier").unwrap_or_default(),
        team_id: field(&out.stderr, "TeamIdentifier").unwrap_or_default(),
        notarized: field(&out.stderr, "Notarization Ticket")
            .is_some_and(|v| v.eq_ignore_ascii_case("stapled")),
    })
}

/// Gatekeeper's own verdict on an app bundle — the notarization gate.
///
/// [`read_signature`] *reports* notarization but nothing rejects on it: a
/// user-installed bundle already faced Gatekeeper at first launch because the
/// browser download carried a quarantine xattr. A programmatic download has no
/// such backstop, so before installing, the caller must ask the same policy
/// engine Gatekeeper uses — against the **bundle** (tickets staple at bundle
/// level; the inner binary's `codesign -dvv` line alone is weaker evidence).
/// This is also the only gate that consults certificate revocation.
///
/// A stapled ticket validates offline. A notarized-but-unstapled bundle needs
/// Apple's servers; a just-published release whose ticket has not propagated
/// fails here with the verbatim `spctl` reason — a clear, retryable error.
pub fn verify_notarized_bundle(bundle: &Path) -> Result<(), TrustError> {
    let out = run_bounded(
        Path::new("/usr/sbin/spctl"),
        &[
            "--assess",
            "--type",
            "execute",
            &bundle.display().to_string(),
        ],
        VERIFY_TIMEOUT,
    )?;
    if out.success() {
        return Ok(());
    }
    Err(TrustError::NotNotarized {
        path: bundle.to_path_buf(),
        detail: first_meaningful_line(&out.stderr),
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

pub(crate) fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no detail reported")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Executable=/Applications/Some.app/Contents/MacOS/some\n\
Identifier=com.example.some\n\
Authority=Developer ID Application: Example, Inc. (TEAMID9999)\n\
Notarization Ticket=stapled\n\
Info.plist entries=15\n\
TeamIdentifier=TEAMID9999\n";

    #[test]
    fn parses_the_identity_fields_codesign_actually_emits() {
        assert_eq!(field(SAMPLE, "Identifier").as_deref(), Some("com.example.some"));
        assert_eq!(field(SAMPLE, "TeamIdentifier").as_deref(), Some("TEAMID9999"));
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
        let reversed = "TeamIdentifier=WRONGTEAM\nIdentifier=com.example.some\n";
        assert_eq!(
            field(reversed, "Identifier").as_deref(),
            Some("com.example.some")
        );
        assert_eq!(
            field(reversed, "TeamIdentifier").as_deref(),
            Some("WRONGTEAM")
        );
    }

    #[test]
    fn missing_fields_are_absent_rather_than_empty() {
        assert!(field("Format=app bundle\n", "TeamIdentifier").is_none());
    }

    #[test]
    fn unstapled_notarization_is_not_reported_as_notarized() {
        let unstapled = "Identifier=com.example.some\nTeamIdentifier=TEAMID9999\n";
        assert!(field(unstapled, "Notarization Ticket").is_none());
    }

    #[test]
    fn an_apple_or_adhoc_identity_is_not_pinnable() {
        // `not set` is a real, non-empty value codesign emits for Apple's own
        // and ad-hoc-signed binaries — the case that proves non-emptiness is
        // not a trust check.
        let apple = Signature {
            identifier: "com.apple.echo".into(),
            team_id: "not set".into(),
            notarized: false,
        };
        assert!(!apple.pinnable());
        let unsigned = Signature {
            identifier: String::new(),
            team_id: String::new(),
            notarized: false,
        };
        assert!(!unsigned.pinnable());
        let real = Signature {
            identifier: "com.example.some".into(),
            team_id: "TEAMID9999".into(),
            notarized: true,
        };
        assert!(real.pinnable());
    }

    #[test]
    fn first_meaningful_line_skips_blanks() {
        assert_eq!(
            first_meaningful_line("\n\n  code object is not signed at all\nmore\n"),
            "code object is not signed at all"
        );
        assert_eq!(first_meaningful_line("   \n"), "no detail reported");
    }

    /// The gate must *reject*, not merely report. An unsigned scratch bundle
    /// is the strongest fixture buildable in a test: it fails Gatekeeper
    /// assessment for the same terminal reason an unnotarized one does (no
    /// ticket, no accepted signature), proving the error path is a hard stop.
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
        assert!(matches!(err, TrustError::NotNotarized { .. }), "got {err:?}");
    }
}
