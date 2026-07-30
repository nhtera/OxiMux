//! Shared macOS trust primitives: code-signature verification and crash-safe
//! bundle placement.
//!
//! Two consumers with the same security problem: the computer-use driver
//! installer and the app's own updater both download a bundle programmatically
//! — so it never carries a quarantine xattr and Gatekeeper never inspects it —
//! and must therefore run the equivalent gates themselves before executing or
//! installing anything:
//!
//! 1. `codesign --verify --strict` — the bytes match their signature.
//! 2. `Identifier` + `TeamIdentifier` against a [`SignaturePolicy`] — it came
//!    from the expected publisher, not merely *a* publisher.
//! 3. `spctl --assess` on the bundle — Apple's own policy engine, the only
//!    check that consults notarization and certificate revocation.
//!
//! Signature verification is the gate rather than a pinned hash because both
//! artifacts update routinely: a hash pin goes stale on every release and
//! trains its maintainer to bump it reflexively, which is no gate at all.
//!
//! The placement half ([`swap`]) exists because "replace a bundle that might
//! be in use" has exactly one safe shape on macOS: copy to the same volume,
//! then `renamex_np(RENAME_SWAP)` so there is no instant at which the target
//! path is empty or half-written.

pub mod exec;
pub mod swap;
pub mod verify;

use std::path::PathBuf;
use std::time::Duration;

pub use exec::{run_bounded, Output};
pub use swap::{ditto_copy, dir_size, ensure_disk_space, exchange, free_bytes, DiskShortfall};
pub use verify::{
    read_signature, verify_integrity, verify_notarized_bundle, verify_signed, Signature,
};

/// The publisher a bundle must prove it came from.
///
/// Anyone can produce a validly signed binary; the team pin is what makes a
/// substituted one fail. Callers own the policy: the driver installer pins a
/// third-party team, the app updater pins the identity of the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePolicy {
    pub identifier: String,
    pub team_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },

    #[error("{program} did not respond within {timeout:?}")]
    Timeout { program: String, timeout: Duration },

    #[error("signature check failed for {}: {detail}", path.display())]
    SignatureInvalid { path: PathBuf, detail: String },

    #[error("binary identifies as `{found}`, expected `{expected}`")]
    UnexpectedIdentifier { found: String, expected: String },

    #[error("binary is signed by team `{found}`, expected `{expected}`")]
    UnexpectedTeamId { found: String, expected: String },

    #[error(
        "{} failed Gatekeeper assessment (not notarized?): {detail}",
        path.display()
    )]
    NotNotarized { path: PathBuf, detail: String },

    #[error("ditto failed: {detail}")]
    DittoFailed { detail: String },
}
