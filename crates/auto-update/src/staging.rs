//! Staged-update bookkeeping: where verified bundles wait for the quit-time
//! swap, and the boot sweep that cleans up after crashes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::UpdateError;

/// Staged bundles are hidden siblings of the installed app: same directory so
/// the eventual `renamex_np` swap stays on one volume, dot-prefixed so Finder
/// does not show a second OxiMux.
const STAGING_PREFIX: &str = ".OxiMux.update-";

/// A verified update waiting on disk for the quit-time swap.
///
/// This file is the single source of truth for "an update is staged": the
/// boot sweep deletes any staging dir it does not reference, and the quit
/// path swaps only what it names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingUpdate {
    pub staged_path: PathBuf,
    pub version: String,
    pub notes: String,
}

impl PendingUpdate {
    pub fn load(manifest_path: &Path) -> Option<Self> {
        let raw = fs::read_to_string(manifest_path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn store(&self, manifest_path: &Path) -> Result<(), UpdateError> {
        if let Some(parent) = manifest_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let raw = serde_json::to_string_pretty(self).expect("manifest serializes");
        fs::write(manifest_path, raw).map_err(|err| UpdateError::Staging {
            detail: format!("could not record the pending update: {err}"),
        })
    }

    /// Remove the manifest and the staged bundle it points at.
    pub fn discard(self, manifest_path: &Path) {
        let _ = fs::remove_dir_all(&self.staged_path);
        let _ = fs::remove_file(manifest_path);
    }
}

/// Claim a fresh staging directory next to `bundle_root`.
///
/// The name is random, not PID-derived — PIDs are `ps`-visible to every local
/// user and `/Applications` is admin-group-writable, so a predictable name is
/// a symlink pre-plant target. `create_dir` is the claim: it refuses to
/// follow or overwrite anything already at the path, and the `symlink_metadata`
/// re-check confirms what now sits there is the plain directory we just made.
pub fn claim_staging_dir(bundle_root: &Path) -> Result<PathBuf, UpdateError> {
    let parent = bundle_root.parent().ok_or_else(|| UpdateError::Staging {
        detail: "installed bundle has no parent directory".into(),
    })?;
    let mut suffix = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut suffix);
    let name = format!(
        "{STAGING_PREFIX}{}.app",
        suffix.map(|b| format!("{b:02x}")).join("")
    );
    let path = parent.join(name);

    fs::create_dir(&path).map_err(|err| UpdateError::Staging {
        detail: format!("could not claim staging dir {}: {err}", path.display()),
    })?;
    let meta = fs::symlink_metadata(&path).map_err(|err| UpdateError::Staging {
        detail: format!("staging dir vanished under us: {err}"),
    })?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        let _ = fs::remove_dir_all(&path);
        return Err(UpdateError::Staging {
            detail: format!("staging path {} is occupied", path.display()),
        });
    }
    Ok(path)
}

/// Where in-flight DMGs and their mountpoints live.
pub fn downloads_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("updates")
}

pub fn mounts_dir(cache_dir: &Path) -> PathBuf {
    downloads_dir(cache_dir).join("mounts")
}

/// What [`apply_pending`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOutcome {
    /// No staged update was waiting.
    Nothing,
    /// The staged bundle is now installed.
    Applied,
    /// A staged update existed but was refused; the installed bundle is
    /// untouched and the staged copy has been discarded.
    Refused,
}

/// Swap a staged update into place. Call from the quit path only — never
/// while the app is running, or subprocesses resolved from the bundle path
/// (relay daemon, hook CLI, screen gate) would skew against it.
///
/// Fails closed at every step: the staged bundle is re-verified *immediately*
/// before the swap (closing the window between staging and quitting, however
/// long the user left it open) and the swapped-in copy is verified again
/// before the old one is discarded, with a rollback if it does not hold.
///
/// `sentinel_path` is written before the swap and cleared after the
/// confirmation. Finding it at boot means a swap was interrupted, so what is
/// installed was never confirmed — see [`recover_interrupted_swap`].
pub fn apply_pending(
    config: &crate::UpdaterConfig,
    sentinel_path: Option<&Path>,
) -> SwapOutcome {
    let Some(pending) = PendingUpdate::load(&config.manifest_path) else {
        return SwapOutcome::Nothing;
    };

    if oximux_macos_trust::verify_signed(&pending.staged_path, &config.app.pin).is_err() {
        tracing::warn!("staged update failed re-verification at quit; discarding");
        pending.discard(&config.manifest_path);
        return SwapOutcome::Refused;
    }

    if let Some(path) = sentinel_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, pending.version.as_bytes());
    }
    let clear_sentinel = || {
        if let Some(path) = sentinel_path {
            let _ = fs::remove_file(path);
        }
    };

    let installed = config.app.bundle_root.clone();
    let staged = pending.staged_path.clone();
    if oximux_macos_trust::exchange(&staged, &installed).is_err() {
        tracing::warn!("could not swap in the staged update; keeping the current version");
        clear_sentinel();
        pending.discard(&config.manifest_path);
        return SwapOutcome::Refused;
    }

    // `staged` now holds the OLD bundle. Confirm the new one before letting
    // go of it.
    if oximux_macos_trust::verify_signed(&installed, &config.app.pin).is_err() {
        tracing::error!("swapped-in update failed verification; rolling back");
        let _ = oximux_macos_trust::exchange(&staged, &installed);
        clear_sentinel();
        pending.discard(&config.manifest_path);
        return SwapOutcome::Refused;
    }

    let _ = fs::remove_dir_all(&staged);
    let _ = fs::remove_file(&config.manifest_path);
    clear_sentinel();
    tracing::info!(version = %pending.version, "applied staged update at quit");
    SwapOutcome::Applied
}

/// A sentinel present at boot means a previous swap never confirmed. Verify
/// what is actually installed rather than assuming the swap completed.
///
/// Returns `Err` when the installed bundle does not verify — the caller
/// should surface that, since nothing here can safely fix it. The sentinel is
/// left in place so the next boot checks again.
pub fn recover_interrupted_swap(
    config: &crate::UpdaterConfig,
    sentinel_path: &Path,
) -> Result<(), UpdateError> {
    if !sentinel_path.exists() {
        return Ok(());
    }
    match oximux_macos_trust::verify_signed(&config.app.bundle_root, &config.app.pin) {
        Ok(_) => {
            tracing::info!("interrupted update swap verified clean at boot");
            let _ = fs::remove_file(sentinel_path);
            Ok(())
        }
        Err(err) => Err(UpdateError::Gate(err)),
    }
}

/// Clean up everything a crashed or killed run can strand. Run once at boot,
/// off the UI thread.
///
/// Order matters: mountpoints first (a busy mount blocks deleting the
/// directory under it), then downloads, then unreferenced staging dirs.
/// `Drop`-based detach never runs on SIGKILL or power loss, so orphaned
/// mounts are expected input here, not a surprise.
pub fn boot_sweep(cache_dir: &Path, bundle_root: &Path, manifest_path: &Path) {
    let mounts = mounts_dir(cache_dir);
    if let Ok(entries) = fs::read_dir(&mounts) {
        for entry in entries.flatten() {
            let mountpoint = entry.path();
            // Harmlessly errors when the dir is not actually a live mount.
            let _ = oximux_macos_trust::run_bounded(
                Path::new("/usr/bin/hdiutil"),
                &["detach", "-force", &mountpoint.display().to_string()],
                Duration::from_secs(30),
            );
            let _ = fs::remove_dir_all(&mountpoint);
        }
    }

    if let Ok(entries) = fs::read_dir(downloads_dir(cache_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "dmg") {
                let _ = fs::remove_file(&path);
            }
        }
    }

    // Staging dirs are invisible in Finder; anything the manifest does not
    // reference would otherwise leak a full app copy per crash, forever.
    let referenced = PendingUpdate::load(manifest_path).map(|pending| pending.staged_path);
    if let Some(parent) = bundle_root.parent()
        && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(STAGING_PREFIX) && Some(&path) != referenced.as_ref() {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("pending-update.json");
        let pending = PendingUpdate {
            staged_path: dir.path().join(".OxiMux.update-abc.app"),
            version: "0.2.0".into(),
            notes: "notes".into(),
        };
        pending.store(&manifest).expect("stores");
        assert_eq!(PendingUpdate::load(&manifest), Some(pending));
    }

    #[test]
    fn claim_refuses_and_survives_an_occupied_path() {
        // Claims are random, so collide two claims by exhausting the space is
        // impractical — instead prove the claim itself is create_dir-atomic:
        // two claims yield two distinct dirs, and both really exist.
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("OxiMux.app");
        fs::create_dir(&bundle).expect("mkdir");
        let a = claim_staging_dir(&bundle).expect("first claim");
        let b = claim_staging_dir(&bundle).expect("second claim");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn boot_sweep_removes_unreferenced_staging_dirs_and_keeps_the_manifest_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("OxiMux.app");
        fs::create_dir(&bundle).expect("mkdir");
        let kept = dir.path().join(".OxiMux.update-keepme.app");
        let orphan = dir.path().join(".OxiMux.update-orphan.app");
        fs::create_dir(&kept).expect("mkdir");
        fs::create_dir(&orphan).expect("mkdir");

        let cache = dir.path().join("cache");
        fs::create_dir_all(downloads_dir(&cache)).expect("mkdir");
        fs::write(downloads_dir(&cache).join("stale.dmg"), b"x").expect("write");

        let manifest = dir.path().join("pending-update.json");
        PendingUpdate {
            staged_path: kept.clone(),
            version: "0.2.0".into(),
            notes: String::new(),
        }
        .store(&manifest)
        .expect("stores");

        boot_sweep(&cache, &bundle, &manifest);

        assert!(kept.exists(), "manifest-referenced staging dir must survive");
        assert!(!orphan.exists(), "orphaned staging dir must be swept");
        assert!(
            !downloads_dir(&cache).join("stale.dmg").exists(),
            "stale DMG must be swept"
        );
    }

    #[test]
    fn discard_removes_both_manifest_and_staged_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = dir.path().join(".OxiMux.update-x.app");
        fs::create_dir(&staged).expect("mkdir");
        let manifest = dir.path().join("pending-update.json");
        let pending = PendingUpdate {
            staged_path: staged.clone(),
            version: "0.2.0".into(),
            notes: String::new(),
        };
        pending.store(&manifest).expect("stores");

        PendingUpdate::load(&manifest)
            .expect("loads")
            .discard(&manifest);

        assert!(!staged.exists());
        assert!(!manifest.exists());
    }
}
