//! One background check, start to finish: ask the release feed, prove what
//! comes back, and leave a verified payload staged beside the install.
//!
//! The run ends at [`UpdateStatus::Ready`] — files on disk, install directory
//! untouched. Nothing here replaces anything; that is
//! [`super::staging::apply_pending`]'s job at quit, for the reasons in that
//! module's header.
//!
//! Cancellation is cooperative and checked between steps rather than inside
//! them. The steps are a few seconds each at worst, and a download that has
//! already started is cheaper to finish and discard than to tear down mid-write.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::release::download::{Fetcher, HttpFetcher};
use crate::release::{self, manifest::Manifest, verify};
use crate::windows::{archive, staging};
use crate::{CheckTrigger, UpdateError, UpdateStatus, UpdaterConfig};

/// The triple whose app payload in a release manifest belongs to this build.
/// Set by `build.rs`; the cross-compile-correct answer, where anything derived
/// from the host would name the wrong asset.
pub const TARGET: &str = env!("OXIMUX_TARGET");

/// The minisign key release manifests are verified against, or `None` when this
/// build has none.
///
/// `None` is fail-closed by construction: the only correct answer without a
/// trust root is to refuse, never to fall back to trusting a checksum that came
/// from the same place as the artifact it describes.
pub fn release_public_key() -> Option<&'static str> {
    let key = env!("OXIMUX_RELEASE_PUBKEY");
    (key != "UNSET" && !key.is_empty()).then_some(key)
}

pub fn run(
    config: &UpdaterConfig,
    trigger: CheckTrigger,
    cancel: &Arc<AtomicBool>,
    emit: &dyn Fn(UpdateStatus),
) -> Result<UpdateStatus, UpdateError> {
    run_with(&HttpFetcher, release_public_key(), config, trigger, cancel, emit)
}

/// The pipeline, over an injectable fetcher and trust root.
///
/// The seam exists so the *ordering* of the gates can be tested in-process
/// against hand-made tampering. Ordering is the part of a trust chain that
/// silently rots: a gate that runs after the thing it was supposed to guard is
/// still a gate, still passes its own unit test, and protects nothing.
pub fn run_with(
    fetcher: &dyn Fetcher,
    public_key: Option<&str>,
    config: &UpdaterConfig,
    trigger: CheckTrigger,
    cancel: &Arc<AtomicBool>,
    emit: &dyn Fn(UpdateStatus),
) -> Result<UpdateStatus, UpdateError> {
    let cancelled = || cancel.load(Ordering::SeqCst);

    emit(UpdateStatus::Checking { trigger });

    // Gate 1: the signature over the manifest, before a single field of it is
    // parsed. Everything downstream is read out of bytes this proved.
    let manifest: Manifest =
        release::fetch_verified_manifest(fetcher, public_key).map_err(UpdateError::from)?;
    if cancelled() {
        return Err(UpdateError::Cancelled);
    }

    // Gate 2: strictly newer. A validly signed *old* release must not be
    // replayable to walk an installation backwards onto a fixed bug.
    let offered = match verify::verify_is_upgrade(&manifest.version, &config.current_version.to_string())
    {
        Ok(version) => version,
        Err(verify::VerifyError::NotAnUpgrade { .. }) => return Ok(UpdateStatus::UpToDate),
        Err(err) => return Err(UpdateError::from(release::ReleaseError::Verify(err))),
    };

    // A staged payload for this exact version is already on disk — the user
    // simply has not quit yet. Re-downloading a hundred megabytes to arrive at
    // the same answer is the one thing a repeat check must not do.
    if let Some(pending) = staging::PendingUpdate::load(&config.manifest_path)
        && pending.version == offered.to_string()
        && pending.staged_path.is_dir()
    {
        return Ok(UpdateStatus::Ready { version: pending.version, notes: pending.notes });
    }

    let asset = manifest
        .app_for(TARGET)
        .map_err(|err| UpdateError::from(release::ReleaseError::Manifest(err)))?;

    emit(UpdateStatus::Downloading {
        version: offered.to_string(),
        received: 0,
        total: Some(asset.size),
    });
    // Gate 3: the digest the signed manifest declares. `fetch_verified_asset`
    // will not hand back bytes that fail it.
    let bytes =
        release::fetch_verified_asset(fetcher, &manifest.tag(), asset).map_err(UpdateError::from)?;
    emit(UpdateStatus::Downloading {
        version: offered.to_string(),
        received: asset.size,
        total: Some(asset.size),
    });
    if cancelled() {
        return Err(UpdateError::Cancelled);
    }

    emit(UpdateStatus::Installing { version: offered.to_string() });

    // A previous run's payload for a *different* version is dead weight the
    // moment a newer one is offered; clearing it first also keeps the parent
    // directory from accumulating one staging dir per release.
    if let Some(stale) = staging::PendingUpdate::load(&config.manifest_path) {
        stale.discard(&config.manifest_path);
    }

    let staged = staging::claim_staging_dir(&config.app.install_dir)?;
    match stage(&bytes, &staged, &offered.to_string()) {
        Ok(()) => {}
        Err(err) => {
            // Never leave a half-extracted directory behind: the next boot
            // sweep would clear it anyway, but a failure that leaves nothing
            // is the one that cannot be half-mistaken for success.
            let _ = std::fs::remove_dir_all(&staged);
            return Err(err);
        }
    }

    let pending = staging::PendingUpdate {
        staged_path: staged,
        version: offered.to_string(),
        notes: manifest_notes(&manifest),
    };
    pending.store(&config.manifest_path)?;

    Ok(UpdateStatus::Ready { version: pending.version, notes: pending.notes })
}

/// Extract and record, in that order. The receipt is written last, so a
/// staging directory carrying one is by construction a complete payload.
fn stage(bytes: &[u8], staged: &Path, version: &str) -> Result<(), UpdateError> {
    let files = archive::extract(bytes, staged).map_err(UpdateError::from)?;
    staging::write_receipt(staged, version, &files)
}

/// The manifest carries no release notes — it is a signed inventory of
/// artifacts, and adding prose to it would put user-facing copy inside the one
/// document whose every field is security-relevant. The What's New popover
/// links out instead.
fn manifest_notes(manifest: &Manifest) -> String {
    format!(
        "OxiMux {} is ready to install.\n\nRelease notes: \
         https://github.com/nhtera/OxiMux/releases/tag/{}",
        manifest.version,
        manifest.tag()
    )
}
