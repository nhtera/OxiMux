//! Where a verified Windows payload waits for the quit-time swap, and what
//! happens to it at boot, at quit, and after a crash.
//!
//! # Why the swap waits for quit
//!
//! The same reason macOS waits: several things this app spawns are resolved
//! from the install path *at spawn time* — the relay daemon, the `agent-status`
//! hook CLI embedded into live agent sessions, the screen-control gate.
//! Replacing files under a running process would leave an old-version app
//! spawning new-version helpers, a skew with no error path because each side is
//! individually valid.
//!
//! Windows adds a second, blunter reason: it refuses to overwrite a mapped
//! image at all. `oximux.exe` and every DLL beside it are mapped for as long as
//! the process lives, and the "move aside, then move in" swap in
//! [`crate::release::swap`] is what makes the replacement possible even then.
//!
//! # What is trusted, and when
//!
//! The trust root is the minisign signature over the release manifest, checked
//! against a key compiled into this binary at the moment of staging. It is
//! **not** re-checked at quit, and the reason is worth stating plainly rather
//! than implying a guarantee that is not there: an attacker who could rewrite
//! the staged payload between staging and quit could equally well rewrite
//! `oximux.exe` in the install directory directly, since a per-user install
//! under `%LOCALAPPDATA%\Programs` is writable by exactly one account. There is
//! no privilege boundary for a re-verification to defend. (macOS pins a
//! codesign identity because `/Applications` is admin-group-writable, which is
//! a genuinely different threat model, not a stricter version of this one.)
//!
//! What *is* re-checked at quit is integrity: every staged file is re-hashed
//! against the digests recorded when it was written. That catches the failure
//! that actually happens — a half-swept staging directory, a disk that filled
//! during extraction, an antivirus quarantine that removed one DLL — and it
//! catches it before anything in the install directory has been touched.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::release::swap::{self, Replacement};
use crate::release::verify::sha256_hex;
use crate::release::ReleaseError;
use crate::UpdateError;

/// Staged payloads are hidden siblings of the install directory: the same
/// parent, so every rename in the swap stays on one filesystem, and
/// dot-prefixed so a user browsing `%LOCALAPPDATA%\Programs` does not find a
/// second OxiMux folder and wonder which one runs.
const STAGING_PREFIX: &str = ".OxiMux.update-";

/// Written inside a staging directory once its payload is complete.
///
/// Its presence is what separates a finished staging directory from one that
/// was interrupted mid-extraction, and the digests inside it are what the quit
/// path re-checks.
const RECEIPT: &str = ".oximux-staged.json";

/// A verified update waiting on disk for the quit-time swap.
///
/// The pending-update file in the app data directory is the single source of
/// truth for "an update is staged": the boot sweep deletes any staging
/// directory it does not reference, and the quit path swaps only what it names.
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

    /// Remove the pending-update file and the staged payload it points at.
    pub fn discard(self, manifest_path: &Path) {
        let _ = fs::remove_dir_all(&self.staged_path);
        let _ = fs::remove_file(manifest_path);
    }
}

/// What a completed staging directory holds, written last so that finding it
/// means everything before it succeeded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Receipt {
    version: String,
    /// Path relative to the staging directory → sha256 of the bytes written.
    files: BTreeMap<String, String>,
}

/// Claim a fresh staging directory beside `install_dir`.
///
/// The name is random rather than derived from the PID: PIDs are enumerable by
/// every process on the machine, and a predictable name in a directory an
/// attacker can also write is a pre-plant target. `create_dir` is the claim —
/// it refuses to follow or overwrite anything already there.
pub fn claim_staging_dir(install_dir: &Path) -> Result<PathBuf, UpdateError> {
    let parent = install_dir.parent().ok_or_else(|| UpdateError::Staging {
        detail: "the install directory has no parent to stage beside".into(),
    })?;
    let mut suffix = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
    let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let path = parent.join(format!("{STAGING_PREFIX}{hex}"));

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

/// Record what was staged. Call once, after the last file is written — the
/// receipt's presence is the "this payload is complete" marker.
pub fn write_receipt(
    staged: &Path,
    version: &str,
    files: &[PathBuf],
) -> Result<(), UpdateError> {
    let mut digests = BTreeMap::new();
    for relative in files {
        let bytes = fs::read(staged.join(relative)).map_err(|err| UpdateError::Staging {
            detail: format!("could not re-read {}: {err}", relative.display()),
        })?;
        digests.insert(relative.to_string_lossy().to_string(), sha256_hex(&bytes));
    }
    let receipt = Receipt { version: version.to_string(), files: digests };
    let raw = serde_json::to_string(&receipt).expect("receipt serializes");
    fs::write(staged.join(RECEIPT), raw).map_err(|err| UpdateError::Staging {
        detail: format!("could not record the staged payload: {err}"),
    })
}

/// Where in-flight downloads live.
pub fn downloads_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("updates")
}

/// What [`apply_pending`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOutcome {
    /// No staged update was waiting.
    Nothing,
    /// The staged payload is now installed.
    Applied,
    /// A staged update existed but was refused; the installed files are
    /// untouched and the staged copy has been discarded.
    Refused,
}

/// Swap a staged update into place. Call from the quit path only.
///
/// Fails closed at every step. A staged payload that no longer matches its
/// receipt is discarded and the running version stays installed; a swap that
/// cannot complete rolls every file back. The one thing this cannot undo is a
/// rollback that itself fails, which is why that case is reported rather than
/// swallowed — see [`swap::SwapError::Inconsistent`].
///
/// Backups the swap could not delete (Windows will not unlink a mapped image,
/// and this process is running out of the very files being replaced) are left
/// for [`boot_sweep`] at the next launch.
pub fn apply_pending(config: &crate::UpdaterConfig, manifest_path: &Path) -> SwapOutcome {
    let Some(pending) = PendingUpdate::load(manifest_path) else {
        return SwapOutcome::Nothing;
    };
    let install_dir = &config.app.install_dir;

    let files = match verified_payload(&pending) {
        Ok(files) => files,
        Err(err) => {
            tracing::warn!(%err, staged = %pending.staged_path.display(),
                           "discarding a staged update that no longer verifies");
            pending.discard(manifest_path);
            return SwapOutcome::Refused;
        }
    };

    let replacements: Vec<Replacement> = files
        .iter()
        .map(|relative| Replacement {
            installed: install_dir.join(relative),
            staged: pending.staged_path.join(relative),
        })
        .collect();

    match swap::swap_all(&replacements) {
        Ok(left_behind) => {
            tracing::info!(
                version = %pending.version,
                files = replacements.len(),
                deferred = left_behind.len(),
                "applied a staged update"
            );
            // The staging directory is now empty of everything but its
            // receipt; the pending file must go with it, or the next boot
            // would try to apply an update that is already installed.
            pending.discard(manifest_path);
            SwapOutcome::Applied
        }
        Err(err) => {
            tracing::error!(%err, "could not apply the staged update");
            // Deliberately kept, not discarded: a rolled-back swap changed
            // nothing, and the payload is still verified and still valid for
            // the next quit. A failure here is usually the install directory
            // being briefly busy, which is not a reason to re-download.
            SwapOutcome::Refused
        }
    }
}

/// Re-hash every staged file against the receipt written when it was extracted,
/// returning the relative paths to swap.
fn verified_payload(pending: &PendingUpdate) -> Result<Vec<PathBuf>, ReleaseError> {
    let raw = fs::read_to_string(pending.staged_path.join(RECEIPT)).map_err(|err| {
        ReleaseError::Staging { detail: format!("the staged update has no receipt: {err}") }
    })?;
    let receipt: Receipt = serde_json::from_str(&raw).map_err(|err| ReleaseError::Staging {
        detail: format!("the staged update's receipt is unreadable: {err}"),
    })?;
    if receipt.version != pending.version {
        return Err(ReleaseError::Staging {
            detail: format!(
                "the staged payload is {} but {} was recorded as pending",
                receipt.version, pending.version
            ),
        });
    }

    let mut files = Vec::with_capacity(receipt.files.len());
    for (relative, expected) in &receipt.files {
        let path = pending.staged_path.join(relative);
        let bytes = fs::read(&path).map_err(|err| ReleaseError::Staging {
            detail: format!("the staged {relative} is unreadable: {err}"),
        })?;
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(ReleaseError::Staging {
                detail: format!("the staged {relative} changed after it was verified"),
            });
        }
        files.push(PathBuf::from(relative));
    }
    if files.is_empty() {
        return Err(ReleaseError::Staging { detail: "the staged payload is empty".into() });
    }
    Ok(files)
}

/// Boot housekeeping, in the order it has to run.
///
/// 1. Delete swap backups a previous quit could not remove, because this
///    process was running out of the files being replaced. This is the second
///    half of every Windows swap, and without it the install directory grows a
///    `.old-` copy of every DLL on every update.
/// 2. Delete staging directories no pending-update file references — the
///    leftovers of a run that was killed mid-download.
///
/// Best-effort throughout: a file that will not delete is a reason to leave it
/// for next time, never a reason to fail a launch.
pub fn boot_sweep(install_dir: &Path, manifest_path: &Path) {
    // Restore-or-sweep, not a plain sweep: a quit killed between the swap's two
    // passes leaves a file moved aside with no replacement in its place, and
    // deleting that backup would destroy the only copy. The app owns its whole
    // install directory, so every `.old-` file in it is ours to act on —
    // unlike the CLI, which shares `~/.local/bin` with the rest of the machine.
    swap::restore_or_sweep_backups(install_dir, |_| true);

    let keep = PendingUpdate::load(manifest_path).map(|pending| pending.staged_path);
    let Some(parent) = install_dir.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX) {
            continue;
        }
        if keep.as_deref() == Some(path.as_path()) {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }

    // A pending file pointing at a staging directory that is no longer there
    // would make the next quit look for a payload that cannot be found. Clear
    // it here instead, so "an update is ready" never outlives the update.
    if let Some(staged) = keep
        && !staged.is_dir()
    {
        let _ = fs::remove_file(manifest_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::install::InstalledApp;
    use crate::Version;

    /// A fake install directory and a staged payload beside it, in one parent
    /// so the renames stay on one filesystem exactly as the real thing does.
    struct Fixture {
        _root: tempfile::TempDir,
        install: PathBuf,
        manifest: PathBuf,
    }

    fn fixture(files: &[(&str, &str)]) -> (Fixture, PendingUpdate) {
        let root = tempfile::tempdir().expect("tempdir");
        let install = root.path().join("OxiMux");
        fs::create_dir(&install).expect("install dir");
        for (name, _) in files {
            fs::write(install.join(name), "v1").expect("installed file");
        }

        let staged = claim_staging_dir(&install).expect("claims");
        let mut written = Vec::new();
        for (name, contents) in files {
            fs::write(staged.join(name), contents).expect("staged file");
            written.push(PathBuf::from(name));
        }
        write_receipt(&staged, "0.2.0", &written).expect("receipt");

        let pending = PendingUpdate {
            staged_path: staged,
            version: "0.2.0".into(),
            notes: "notes".into(),
        };
        let manifest = root.path().join("pending-update.json");
        pending.store(&manifest).expect("stores");

        (Fixture { _root: root, install, manifest }, pending)
    }

    fn config(install: &Path) -> crate::UpdaterConfig {
        crate::UpdaterConfig {
            current_version: Version::new(0, 1, 0),
            app: InstalledApp { install_dir: install.to_path_buf() },
            cache_dir: std::env::temp_dir(),
            manifest_path: PathBuf::from("unused"),
        }
    }

    #[test]
    fn a_verified_payload_replaces_every_installed_file() {
        let (fx, pending) = fixture(&[("oximux.exe", "v2"), ("onnxruntime.dll", "native v2")]);

        assert_eq!(apply_pending(&config(&fx.install), &fx.manifest), SwapOutcome::Applied);
        assert_eq!(fs::read_to_string(fx.install.join("oximux.exe")).expect("read"), "v2");
        assert_eq!(
            fs::read_to_string(fx.install.join("onnxruntime.dll")).expect("read"),
            "native v2"
        );
        assert!(!fx.manifest.exists(), "an applied update stops being pending");
        assert!(!pending.staged_path.exists(), "and its staging directory is gone");
    }

    /// The check the receipt exists for. A staged file that changed after it
    /// was verified must stop the swap *before* anything is renamed — the
    /// install directory has to still be intact when this is caught.
    #[test]
    fn a_staged_file_that_changed_is_refused_and_nothing_is_touched() {
        let (fx, pending) = fixture(&[("oximux.exe", "v2")]);
        fs::write(pending.staged_path.join("oximux.exe"), "tampered").expect("tamper");

        assert_eq!(apply_pending(&config(&fx.install), &fx.manifest), SwapOutcome::Refused);
        assert_eq!(
            fs::read_to_string(fx.install.join("oximux.exe")).expect("read"),
            "v1",
            "the running version must stay installed"
        );
        assert!(!fx.manifest.exists(), "and the bad payload stops being pending");
    }

    /// Extraction that died before the receipt was written leaves a directory
    /// full of plausible-looking files. Without the receipt it must not be
    /// mistaken for a finished payload.
    #[test]
    fn a_staging_directory_with_no_receipt_is_not_a_payload() {
        let (fx, pending) = fixture(&[("oximux.exe", "v2")]);
        fs::remove_file(pending.staged_path.join(RECEIPT)).expect("remove receipt");

        assert_eq!(apply_pending(&config(&fx.install), &fx.manifest), SwapOutcome::Refused);
        assert_eq!(fs::read_to_string(fx.install.join("oximux.exe")).expect("read"), "v1");
    }

    #[test]
    fn no_pending_file_means_there_is_nothing_to_do() {
        let root = tempfile::tempdir().expect("tempdir");
        let install = root.path().join("OxiMux");
        fs::create_dir(&install).expect("install dir");
        let outcome = apply_pending(&config(&install), &root.path().join("absent.json"));
        assert_eq!(outcome, SwapOutcome::Nothing);
    }

    /// The sweep is the second half of every Windows swap. Leaving these
    /// behind means the install directory grows a copy of every DLL per update.
    #[test]
    fn the_boot_sweep_clears_backups_and_abandoned_staging_dirs() {
        let (fx, pending) = fixture(&[("oximux.exe", "v2")]);
        let backup = fx.install.join("onnxruntime.dll.old-deadbeef");
        fs::write(&backup, "old").expect("backup");
        let abandoned = claim_staging_dir(&fx.install).expect("claims a second");

        boot_sweep(&fx.install, &fx.manifest);

        assert!(!backup.exists(), "an undeletable backup is swept at the next boot");
        assert!(!abandoned.exists(), "a staging dir nothing references is swept");
        assert!(pending.staged_path.is_dir(), "the referenced one survives");
        assert!(fx.manifest.exists(), "and so does the pending record");
    }

    /// "An update is ready" must never outlive the update itself, or the pill
    /// in the title bar promises a restart that would do nothing.
    #[test]
    fn a_pending_record_whose_payload_vanished_is_cleared() {
        let (fx, pending) = fixture(&[("oximux.exe", "v2")]);
        fs::remove_dir_all(&pending.staged_path).expect("remove");

        boot_sweep(&fx.install, &fx.manifest);
        assert!(!fx.manifest.exists());
    }

    #[test]
    fn staging_directories_do_not_collide() {
        let root = tempfile::tempdir().expect("tempdir");
        let install = root.path().join("OxiMux");
        fs::create_dir(&install).expect("install dir");
        let a = claim_staging_dir(&install).expect("first");
        let b = claim_staging_dir(&install).expect("second");
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(root.path()), "staged beside the install, not inside it");
    }
}
