//! The four steps of an install that no two platforms agree on.
//!
//! Everything else in `install/` is shared: resolving the release, streaming
//! the download, hashing it as it lands, progress, cancellation, the byte
//! ceiling, the one-install-per-process lock. What differs is narrow and is
//! collected here rather than spread through `pipeline.rs` as `cfg` islands:
//!
//! | step | macOS | Windows |
//! |---|---|---|
//! | archive | `.tar.gz` carrying `CuaDriver.app` | `.zip` of bare executables |
//! | extract | `/usr/bin/tar` | the `zip` crate, in-process |
//! | gate | codesign → identifier → team ID → stapled ticket | nothing automatic; a person |
//! | place | `renamex_np` exchange in `/Applications` | junction retarget over a versioned dir |
//!
//! # Why a `cfg` pair and not a trait
//!
//! Two implementations that are never both live do not need dynamic dispatch,
//! and a trait would put the *shape* in one place while hiding the *reasons* in
//! another. The reasons are the valuable part — see each recipe's module doc.
//!
//! # Linux
//!
//! Upstream publishes Linux archives, but this crate has never been compiled
//! for Linux (`oximux-computer-use` is not in the Linux CI crate set, and the
//! desktop app ships for macOS and Windows only). A third recipe would be dead
//! code that no host builds, so the seam is shaped for it and the file is
//! deliberately absent. The `compile_error!` below is what a Linux build would
//! hit: an explicit "write the recipe", not a pile of missing symbols.

use std::path::PathBuf;

use crate::verify::VerifiedDriver;
use crate::version::Version;

use super::InstallError;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::{extract, gate, installed_version, place, record_approval, verify_placed};

#[cfg(windows)]
mod junction;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::{extract, gate, installed_version, place, record_approval, verify_placed};

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!(
    "no driver-install recipe for this platform — add install/platform/<os>.rs \
     (asset name, extract, gate, place, installed_version) before enabling \
     computer use here"
);

/// What the caller must supply for the gate to have an answer.
///
/// On macOS the anchor is the platform's own certificate chain, which the
/// process already has — so there is nothing to pass. On Windows there is no
/// ambient anchor at all, so the caller has to name whose approval the install
/// will rely on. That is the same argument [`crate::prepare`] makes with its
/// differing signature, and it is deliberate rather than an inconvenience: an
/// installer that invented its own answer would be inventing the trust model.
#[cfg(windows)]
pub type Anchor = crate::trust::TrustStore;
#[cfg(not(windows))]
pub type Anchor = ();

/// An extracted, not-yet-placed driver.
#[derive(Debug)]
pub(super) struct Staged {
    /// What placement moves and what a bundle-level gate inspects: the `.app`
    /// on macOS, the directory of executables on Windows.
    pub(super) root: PathBuf,
    /// The executable whose bytes are the driver's identity.
    pub(super) binary: PathBuf,
    /// The version the release feed *claimed*. A claim, not proof — on macOS
    /// the gate replaces it with the version the binary reports, and on Windows
    /// nothing may replace it before placement, because reading a version means
    /// executing a binary nobody has approved yet.
    pub(super) claimed: Version,
}

/// The gate's verdict on staged bytes.
pub(super) enum Gate {
    /// The platform answered "authentic" on its own.
    Passed(VerifiedDriver),
    /// The platform cannot answer. These bytes need a person — see
    /// [`crate::trust`] for what that approval does and does not establish.
    NeedsApproval {
        sha256: String,
        version: Version,
        bytes: u64,
    },
}

/// The macOS archive: the one carrying `CuaDriver.app`.
///
/// The flat `-binary` variant published beside it is deliberately not used —
/// [`crate::daemon`] depends on the driver being a real bundle, because that is
/// the identity macOS attributes TCC grants to.
///
/// Compiled — and public — on every host so the feed parser can be tested
/// against both platforms' asset names from one fixture.
pub fn macos_asset_name(version: &Version) -> String {
    format!("cua-driver-rs-{version}-darwin-universal.tar.gz")
}

/// The Windows archive: the flat `-binary` zip.
///
/// Preferred over the directory zip published beside it, whose entries all sit
/// under a `cua-driver-rs-<version>-<arch>/` prefix that a consumer has to
/// reconstruct by hand — upstream's own installer does exactly that and gives
/// up when its guess misses.
///
/// Compiled on every host; see [`macos_asset_name`].
pub fn windows_asset_name(version: &Version) -> String {
    format!("cua-driver-rs-{version}-windows-{}-binary.zip", win_arch())
}

/// Upstream's short arch label for Windows assets.
fn win_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x86_64",
    }
}

/// The archive this platform installs from. Public because the live-feed test
/// resolves a real release through it.
pub fn asset_name(version: &Version) -> String {
    #[cfg(windows)]
    {
        windows_asset_name(version)
    }
    #[cfg(not(windows))]
    {
        macos_asset_name(version)
    }
}

/// Shared failure for "the archive did not contain what its name promises".
pub(super) fn missing_from_archive(expected: &str, listing: Vec<String>) -> InstallError {
    InstallError::ArchiveIncomplete {
        listing: format!("expected {expected}; archive held: {}", listing.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_follow_upstreams_published_shapes() {
        let version = Version::new(0, 21, 0);
        assert_eq!(
            macos_asset_name(&version),
            "cua-driver-rs-0.21.0-darwin-universal.tar.gz"
        );
        // Arch varies with the host; the shape does not.
        let windows = windows_asset_name(&version);
        assert!(windows.starts_with("cua-driver-rs-0.21.0-windows-"));
        assert!(windows.ends_with("-binary.zip"));
    }

    #[test]
    fn this_platforms_asset_is_one_of_the_two() {
        let version = Version::new(0, 21, 0);
        let name = asset_name(&version);
        assert!(name == macos_asset_name(&version) || name == windows_asset_name(&version));
    }
}
