//! The macOS recipe: a notarized `.app`, gated on Apple's signature.
//!
//! Every step here was previously inline in `pipeline.rs`; the behaviour is
//! unchanged. What the move buys is that the Windows recipe sits beside it as a
//! peer rather than as a `cfg` branch threaded through shared code.
//!
//! The gate is automatic and complete: `codesign --verify --strict`, the pinned
//! identifier and Team ID, and Gatekeeper's own bundle-level notarization
//! verdict — necessary because a programmatic download carries no quarantine
//! xattr, so nothing else would ever ask Gatekeeper about it. See
//! [`crate::verify`] for why that is the gate rather than a pinned hash.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::place::{self, Swap, APP_NAME};
use super::super::InstallError;
use super::{missing_from_archive, Anchor, Gate, Staged};
use crate::exec;
use crate::verify::{self, VerifiedDriver};
use crate::version::Version;

/// Executable inside the bundle.
const APP_BINARY: &str = "Contents/MacOS/cua-driver";

const TAR_TIMEOUT: Duration = Duration::from_secs(120);

/// A completed swap awaiting the post-place verification verdict.
pub(in crate::install) struct Placement {
    swap: Swap,
}

impl Placement {
    /// What the post-place gate re-runs against.
    pub(in crate::install) fn binary(&self) -> PathBuf {
        self.swap.target.join(APP_BINARY)
    }

    pub(in crate::install) fn commit(self) {
        self.swap.commit();
    }

    pub(in crate::install) fn roll_back(self) {
        self.swap.roll_back();
    }
}

/// `/usr/bin/tar` by absolute path — no `PATH` lookup for anything that touches
/// the downloaded artifact.
///
/// Deliberately not the `tar` crate: a signed bundle's integrity depends on
/// symlinks, permission bits and extended attributes surviving extraction
/// exactly, and the gate that reads them runs seconds later. This is not the
/// place to trade a working guarantee for a dependency.
pub(in crate::install) fn extract(
    archive: &Path,
    into: &Path,
    claimed: Version,
) -> Result<Staged, InstallError> {
    let out = exec::run_bounded(
        Path::new("/usr/bin/tar"),
        &[
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &into.display().to_string(),
        ],
        TAR_TIMEOUT,
    )?;
    if !out.success() {
        return Err(InstallError::Install {
            detail: format!("tar failed: {}", out.stderr.lines().next().unwrap_or("")),
        });
    }

    let root = find_app(into)?;
    let binary = root.join(APP_BINARY);
    Ok(Staged {
        root,
        binary,
        claimed,
    })
}

/// Identity gates first (no execution beyond `verify.rs`'s bounded
/// `--version`), then Gatekeeper's bundle-level notarization assessment.
pub(in crate::install) fn gate(staged: &Staged, _anchor: &Anchor) -> Result<Gate, InstallError> {
    let driver = verify::verify(&staged.binary)?;
    verify::verify_notarized_bundle(&staged.root)?;
    Ok(Gate::Passed(driver))
}

/// macOS never asks a person, so nothing ever calls this.
///
/// It exists to keep the pipeline one shape rather than two. Reaching it would
/// mean [`gate`] had returned `NeedsApproval` on a platform with no approval to
/// record — a bug, and one that must fail the install rather than quietly place
/// bytes nothing vouched for.
pub(in crate::install) fn record_approval(_: &Staged, _: &Anchor) -> Result<(), InstallError> {
    Err(InstallError::Install {
        detail: "macOS installs are gated on the signature; there is no approval to record".into(),
    })
}

pub(in crate::install) fn place(staged: &Staged) -> Result<Placement, InstallError> {
    place::swap_in(&staged.root).map(|swap| Placement { swap })
}

/// Prove that what landed is what was gated.
pub(in crate::install) fn verify_placed(
    binary: &Path,
    _anchor: &Anchor,
) -> Result<VerifiedDriver, crate::Error> {
    verify::verify(binary)
}

/// The version of the driver already installed, if one can be trusted enough to
/// read a version off.
pub(in crate::install) fn installed_version() -> Option<Version> {
    match crate::prepare() {
        Ok(driver) => Some(driver.version),
        // A too-old install still has a readable version; upgrading over it is
        // the whole point. Any other state (missing, broken signature) has no
        // trustworthy version to compare against.
        Err(crate::Error::DriverTooOld { found, .. }) => Some(found),
        Err(_) => None,
    }
}

/// Locate `CuaDriver.app` in the extracted tree without hardcoding the staging
/// dir name (upstream layout may drift). Two levels is generous for an archive
/// whose contract is `<staging-dir>/CuaDriver.app`.
fn find_app(root: &Path) -> Result<PathBuf, InstallError> {
    fn scan(dir: &Path, depth: u8, seen: &mut Vec<String>) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == APP_NAME) {
                return Some(path);
            }
            seen.push(entry.file_name().to_string_lossy().into_owned());
            if depth > 0
                && let Some(found) = scan(&path, depth - 1, seen)
            {
                return Some(found);
            }
        }
        None
    }

    let mut seen = Vec::new();
    scan(root, 2, &mut seen).ok_or_else(|| missing_from_archive(APP_NAME, seen))
}
