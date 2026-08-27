//! The signed-release trust chain, shared by every updater in the workspace.
//!
//! Two very different programs update themselves from the same GitHub Release:
//! `oximux update` replaces two CLI binaries, and the desktop app replaces its
//! whole install directory at quit. What they must *not* have is two answers to
//! "is this release genuine" — so the manifest, its signature, the download
//! host allow-list, and the move-aside/move-in swap live here, once.
//!
//! The trust chain, in the order it must run and for the reason it must run in
//! that order:
//!
//! ```text
//! minisign signature over manifest.json   ← the only independent trust root
//!   └─ manifest parsed (never before)
//!        └─ version strictly greater than the running one
//!             └─ artifact fetched, sha256 checked against the signed manifest
//!                  └─ extracted, platform gate, all-or-nothing swap
//! ```
//!
//! The first step is what makes the rest worth anything. A sha256 taken from a
//! manifest published beside the artifact it describes proves only that the
//! download matches what the publisher said; a compromised publish token
//! rewrites both. The signature is checked against a key compiled into the
//! binary, which that token cannot reach.
//!
//! What is deliberately *not* here: how a given program's payload is unpacked
//! and which files it owns. The CLI extracts two names out of a `.tar.gz`; the
//! desktop app unpacks a whole directory out of a `.zip`. Those are the parts
//! that legitimately differ, and each lives with its own consumer.

pub mod download;
pub mod manifest;
pub mod swap;
/// Test-only signing, behind the `testkit` feature — see its note in
/// `Cargo.toml`. Reachable from this crate's own tests without the feature.
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod verify;

use std::path::{Path, PathBuf};

use download::Fetcher;
use manifest::Manifest;

/// A manifest is a few hundred bytes of JSON; a signature is four lines.
/// Ceilings this tight mean a tarpit is refused before it can matter.
pub const MANIFEST_CEILING: u64 = 256 * 1024;
pub const SIGNATURE_CEILING: u64 = 8 * 1024;

/// Everything that can go wrong between "ask for a release" and "the new files
/// are in place".
///
/// One enum for both consumers rather than one each: the variants are about the
/// release, not about who is installing it, and a second copy would drift on
/// exactly the security-relevant ones. How a failure is *rendered* does differ,
/// and that stays with each consumer — the CLI maps these onto its JSON failure
/// envelope, the desktop app onto a line of text in the About pane.
#[derive(Debug)]
pub enum ReleaseError {
    Network { detail: String },
    /// The release exists but has no manifest, or there is no release at all.
    NoRelease,
    RateLimited,
    DisallowedHost { host: String },
    Oversize { ceiling: u64 },
    Archive { detail: String },
    Staging { detail: String },
    /// Installed by a package manager that owns the files.
    ManagedInstall { manager: &'static str, command: String },
    Manifest(manifest::ManifestError),
    Verify(verify::VerifyError),
    Swap(swap::SwapError),
}

impl std::fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network { detail } => write!(f, "could not reach the release server: {detail}"),
            Self::NoRelease => write!(f, "no published release carries an update manifest"),
            Self::RateLimited => write!(f, "the release server is rate-limiting this machine"),
            Self::DisallowedHost { host } => {
                write!(f, "the download was redirected to {host}, which is not a release host")
            }
            Self::Oversize { ceiling } => {
                write!(f, "the download exceeded its {ceiling}-byte ceiling and was abandoned")
            }
            Self::Archive { detail } => write!(f, "{detail}"),
            Self::Staging { detail } => write!(f, "{detail}"),
            Self::ManagedInstall { manager, .. } => {
                write!(f, "this oximux was installed by {manager}, which owns the files")
            }
            Self::Manifest(err) => write!(f, "{err}"),
            Self::Verify(err) => write!(f, "{err}"),
            Self::Swap(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl From<manifest::ManifestError> for ReleaseError {
    fn from(err: manifest::ManifestError) -> Self {
        Self::Manifest(err)
    }
}
impl From<verify::VerifyError> for ReleaseError {
    fn from(err: verify::VerifyError) -> Self {
        Self::Verify(err)
    }
}
impl From<swap::SwapError> for ReleaseError {
    fn from(err: swap::SwapError) -> Self {
        Self::Swap(err)
    }
}

/// Fetch the manifest and prove it before parsing a single field of it.
pub fn fetch_verified_manifest(
    fetcher: &dyn Fetcher,
    public_key: Option<&str>,
) -> Result<Manifest, ReleaseError> {
    // Fail on a keyless build before spending two requests on bytes that
    // could never be trusted.
    if public_key.is_none() {
        return Err(verify::VerifyError::NoTrustRoot.into());
    }
    let raw = fetcher.get(&download::manifest_url(), MANIFEST_CEILING)?;
    let signature = fetcher.get(&download::signature_url(), SIGNATURE_CEILING)?;
    let signature = String::from_utf8(signature).map_err(|_| {
        ReleaseError::Verify(verify::VerifyError::MalformedSignature("not text".into()))
    })?;
    verify::verify_manifest_signature(&raw, &signature, public_key)?;
    Ok(Manifest::parse(&raw)?)
}

/// Download one signed artifact and prove its digest before a caller may look
/// at the bytes.
///
/// The ceiling is twice the declared size: generous for transfer overhead,
/// tight enough to stop a stream that intends to never end. The URL is built
/// from the *signed* tag and file name by [`download::asset_url`], never read
/// out of the manifest, so a misused signing key still cannot name a host.
pub fn fetch_verified_asset(
    fetcher: &dyn Fetcher,
    tag: &str,
    asset: &manifest::Asset,
) -> Result<Vec<u8>, ReleaseError> {
    let url = download::asset_url(tag, &asset.archive);
    let bytes = fetcher.get(&url, asset.size.saturating_mul(2).max(1))?;
    verify::verify_digest(&bytes, &asset.sha256)?;
    Ok(bytes)
}

/// A staging directory that removes itself, so a failed verify leaves nothing
/// a later run could mistake for a verified payload.
///
/// Always claimed *beside* the files it will replace, so every rename in the
/// swap stays on one filesystem — a cross-device rename is not atomic and
/// would defeat the all-or-nothing property the swap is built for.
pub struct Staging(PathBuf);

impl Staging {
    pub fn claim(dir: &Path) -> Result<Self, ReleaseError> {
        // Unpredictable, not merely unique: a guessable name in a directory
        // someone else can write is a symlink pre-plant target.
        let mut suffix = [0u8; 6];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
        let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!(".oximux.update-{hex}"));
        std::fs::create_dir(&path).map_err(|err| ReleaseError::Staging {
            detail: format!("could not stage the update in {}: {err}", dir.display()),
        })?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Give up ownership: the directory stays on disk after this value drops.
    ///
    /// For the desktop app, whose staged payload has to survive until the next
    /// quit — the whole point of staging in the background and swapping later.
    pub fn keep(self) -> PathBuf {
        let path = self.0.clone();
        std::mem::forget(self);
        path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
