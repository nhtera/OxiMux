//! What this binary is: version, channel, commit, target triple, and the
//! release key it verifies updates against. All fixed at compile time by
//! `build.rs`.

/// The sentinel a build with no release key compiled in carries. Kept as a
/// word rather than an empty string so a truncated or half-written key file
/// cannot read as "no key needed".
const UNSET: &str = "UNSET";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHANNEL: &str = env!("OXIMUX_BUILD_CHANNEL");
pub const GIT_SHA: &str = env!("OXIMUX_GIT_SHA");
/// The triple whose assets in a release manifest belong to this binary.
pub const TARGET: &str = env!("OXIMUX_TARGET");

/// The minisign public key release manifests are verified against, or `None`
/// when this build has no key.
///
/// `None` is fail-closed by construction: every caller must decide what to do
/// without a trust root, and the only correct answer — the one
/// [`crate::update`] takes — is to refuse the update rather than fall back to
/// trusting a checksum that came from the same place as the artifact.
pub fn release_public_key() -> Option<&'static str> {
    let key = env!("OXIMUX_RELEASE_PUBKEY");
    (key != UNSET && !key.is_empty()).then_some(key)
}

/// `0.1.6 (stable, a1b2c3d4e5f6, aarch64-apple-darwin)` — one line, every
/// build fact a bug report needs.
pub fn describe() -> String {
    format!("{VERSION} ({CHANNEL}, {GIT_SHA}, {TARGET})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_describes_itself_completely() {
        // Each of these comes from build.rs; an empty one means the script
        // stopped emitting it and `oximux version` would print a hole.
        for (name, value) in
            [("version", VERSION), ("channel", CHANNEL), ("sha", GIT_SHA), ("target", TARGET)]
        {
            assert!(!value.is_empty(), "{name} must not be empty");
        }
        assert!(describe().contains(VERSION));
    }

    /// A developer build must not claim to be a channel a user could have
    /// installed from — the channel is what `oximux update` would follow.
    #[test]
    fn an_unreleased_build_is_not_labelled_stable() {
        #[cfg(debug_assertions)]
        assert_eq!(CHANNEL, "dev", "debug builds are never a release channel");
    }

    /// The repo ships no key, so the checked-in state must read as "no trust
    /// root". If this ever fails, a real key landed and this test should be
    /// inverted — but it must never fail *silently* by the sentinel changing
    /// shape and reading as a valid key.
    #[test]
    fn the_sentinel_reads_as_no_key_rather_than_a_key_named_unset() {
        let raw = env!("OXIMUX_RELEASE_PUBKEY");
        if raw == UNSET {
            assert!(release_public_key().is_none());
        } else {
            assert_eq!(release_public_key(), Some(raw));
        }
    }
}
