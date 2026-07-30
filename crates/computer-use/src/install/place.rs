//! Placing a verified `CuaDriver.app` at an install root, crash-safely.
//!
//! Replacing an existing install uses `renamex_np(RENAME_SWAP)` — macOS's
//! atomic two-path exchange — so there is *no instant* at which the target
//! path is empty: before the syscall it holds the old driver, after it the
//! new one, and the old ends up at the staging path awaiting the post-swap
//! verification verdict. A crash can strand a staging dir, but never leaves
//! the user with no driver — the failure mode that would turn an upgrade
//! button into an uninstaller. The daemon is deliberately untouched; a
//! running process keeps the old inode until it respawns.
//!
//! The copy/exchange/disk primitives live in `oximux-macos-trust` (shared
//! with the app updater); this module owns the driver-specific staging and
//! rollback orchestration.

use std::fs;
use std::path::{Path, PathBuf};

use oximux_macos_trust::{ditto_copy, dir_size, ensure_disk_space, exchange, TrustError};

use super::pipeline::APP_NAME;
use super::InstallError;
use crate::discovery;
use crate::verify::from_trust;

/// Headroom past the app's own size — extraction slack plus "the disk was
/// nearly full anyway is its own problem" margin. This repo has ENOSPC
/// history; treat a full disk as expected input, not an exotic failure.
const DISK_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// A completed swap awaiting the post-install verification verdict.
/// `previous` is where the replaced install now lives (the staging path,
/// after the exchange) — `None` on a fresh install.
pub(super) struct Swap {
    pub(super) target: PathBuf,
    previous: Option<PathBuf>,
}

impl Swap {
    /// Final verification passed — the previous install is no longer needed.
    pub(super) fn commit(self) {
        if let Some(previous) = self.previous {
            let _ = fs::remove_dir_all(previous);
        }
    }

    /// Final verification failed — put the previous install back.
    pub(super) fn roll_back(self) {
        match self.previous {
            // Exchange back where the kernel allows it (no-empty-instant);
            // otherwise remove-then-rename — this is already the failure
            // path, and ending with the old driver in place is the contract.
            Some(previous) => {
                if exchange(&self.target, &previous).is_ok() {
                    let _ = fs::remove_dir_all(previous);
                } else {
                    let _ = fs::remove_dir_all(&self.target);
                    let _ = fs::rename(&previous, &self.target);
                }
            }
            None => {
                let _ = fs::remove_dir_all(&self.target);
            }
        }
    }
}

/// Copy the staged bundle next to the target (same filesystem, so the swap
/// that follows is atomic), then swap it in.
pub(super) fn swap_in(staged_app: &Path) -> Result<Swap, InstallError> {
    let root = writable_root()?;
    let needed = dir_size(staged_app) + DISK_MARGIN_BYTES;
    ensure_disk_space(&root, needed).map_err(|shortfall| InstallError::DiskSpace {
        root: root.clone(),
        needed_mb: shortfall.needed_mb,
        free_mb: shortfall.free_mb,
    })?;

    let target = root.join(APP_NAME);
    let staging = root.join(format!("{APP_NAME}.staging-{}", std::process::id()));

    if let Err(err) = ditto_copy(staged_app, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(match err {
            TrustError::DittoFailed { detail } => InstallError::Install {
                detail: format!("ditto failed: {detail}"),
            },
            other => InstallError::Gate(from_trust(other)),
        });
    }

    let result = if target.exists() {
        match exchange(&staging, &target) {
            Ok(()) => Ok(Swap {
                target: target.clone(),
                previous: Some(staging.clone()),
            }),
            // The kernel refuses RENAME_SWAP for some real app bundles
            // (observed: EPERM replacing a provenance-stamped install in
            // /Applications, while plain renames succeed). Fall back to two
            // atomic renames with a `.bak` in between — a theoretical
            // two-syscall window at the target path, still crash-recoverable
            // via the `.bak`.
            Err(_) => two_rename_swap(&staging, &target),
        }
    } else {
        fs::rename(&staging, &target)
            .map(|()| Swap {
                target: target.clone(),
                previous: None,
            })
            .map_err(|err| InstallError::Install {
                detail: format!("could not move the new driver into place: {err}"),
            })
    };
    result.inspect_err(|_| {
        let _ = fs::remove_dir_all(&staging);
    })
}

/// The fallback replace: old aside, new in, both atomic renames. The window
/// between them is two back-to-back syscalls; a crash inside it leaves the
/// previous install recoverable at the returned `previous` path.
fn two_rename_swap(staging: &Path, target: &Path) -> Result<Swap, InstallError> {
    let bak = target.with_extension(format!("bak-{}", std::process::id()));
    fs::rename(target, &bak).map_err(|err| InstallError::Install {
        detail: format!("could not set aside the existing install: {err}"),
    })?;
    match fs::rename(staging, target) {
        Ok(()) => Ok(Swap {
            target: target.to_path_buf(),
            previous: Some(bak),
        }),
        Err(err) => {
            let _ = fs::rename(&bak, target);
            Err(InstallError::Install {
                detail: format!("could not move the new driver into place: {err}"),
            })
        }
    }
}

/// First install root we can actually write to. `/Applications` matches the
/// official installer; `~/Applications` covers non-admin users. Probed by
/// creating a file — directory permission bits under-report on macOS.
fn writable_root() -> Result<PathBuf, InstallError> {
    for root in discovery::install_roots() {
        let probe = root.join(format!(".oximux-write-probe-{}", std::process::id()));
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(&probe);
            return Ok(root);
        }
    }
    Err(InstallError::NoWritableRoot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crash-safety contract: a failed final verification restores the
    /// previous install; a passed one removes it.
    #[test]
    fn roll_back_restores_the_previous_install() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join(APP_NAME);
        let previous = root.path().join("CuaDriver.app.staging-1");
        fs::create_dir(&target).expect("new install in place");
        fs::create_dir(&previous).expect("old install at staging path");
        fs::write(previous.join("marker"), b"old").expect("write");

        Swap {
            target: target.clone(),
            previous: Some(previous.clone()),
        }
        .roll_back();

        assert!(
            fs::read(target.join("marker")).is_ok_and(|bytes| bytes == b"old"),
            "old install must be back at the target path"
        );
        assert!(!previous.exists(), "rejected bundle must be discarded");
    }

    #[test]
    fn commit_discards_the_previous_install() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join(APP_NAME);
        let previous = root.path().join("CuaDriver.app.staging-1");
        fs::create_dir(&target).expect("mkdir");
        fs::create_dir(&previous).expect("mkdir");

        Swap {
            target: target.clone(),
            previous: Some(previous.clone()),
        }
        .commit();

        assert!(target.exists());
        assert!(!previous.exists());
    }
}
