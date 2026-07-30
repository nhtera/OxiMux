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

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::pipeline::APP_NAME;
use super::InstallError;
use crate::{discovery, exec};

const DITTO_TIMEOUT: Duration = Duration::from_secs(180);

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
    ensure_disk_space(&root, staged_app)?;

    let target = root.join(APP_NAME);
    let staging = root.join(format!("{APP_NAME}.staging-{}", std::process::id()));

    // `ditto` preserves signatures, resource forks, and xattrs — `cp -R`
    // famously does not, and a stripped signature would fail the re-verify.
    let out = exec::run_bounded(
        Path::new("/usr/bin/ditto"),
        &[
            &staged_app.display().to_string(),
            &staging.display().to_string(),
        ],
        DITTO_TIMEOUT,
    )?;
    if !out.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(InstallError::Install {
            detail: format!("ditto failed: {}", out.stderr.lines().next().unwrap_or("")),
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

/// Atomically exchange two paths — `renamex_np(RENAME_SWAP)`. Both must
/// exist; afterwards each sits where the other was, with no intermediate
/// state visible to any observer.
#[cfg(target_os = "macos")]
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_a = std::ffi::CString::new(a.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let c_b = std::ffi::CString::new(b.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    if unsafe { libc::renamex_np(c_a.as_ptr(), c_b.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Non-macOS fallback (this crate only ships on macOS; keeps cross-checks
/// compiling): rename through a sibling temp name — a brief empty instant the
/// macOS path does not have.
#[cfg(not(target_os = "macos"))]
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    let hold = b.with_extension("exchange-hold");
    fs::rename(b, &hold)?;
    fs::rename(a, b)?;
    fs::rename(&hold, a)
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

fn ensure_disk_space(root: &Path, staged_app: &Path) -> Result<(), InstallError> {
    let needed = dir_size(staged_app) + DISK_MARGIN_BYTES;
    let Some(free) = free_bytes(root) else {
        // statvfs failing is not evidence of a full disk; the swap itself
        // fails safe (bak restore) if space runs out mid-copy.
        return Ok(());
    };
    if free < needed {
        return Err(InstallError::DiskSpace {
            root: root.to_path_buf(),
            needed_mb: needed / (1024 * 1024),
            free_mb: free / (1024 * 1024),
        });
    }
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            match entry.metadata() {
                Ok(meta) if meta.is_file() => meta.len(),
                Ok(meta) if meta.is_dir() => dir_size(&path),
                _ => 0,
            }
        })
        .sum()
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } == 0;
    ok.then(|| vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_size_sums_nested_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("a"), vec![0u8; 100]).expect("write");
        fs::write(dir.path().join("sub/b"), vec![0u8; 50]).expect("write");
        assert_eq!(dir_size(dir.path()), 150);
    }

    #[test]
    fn free_bytes_reports_something_for_the_temp_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(free_bytes(dir.path()).is_some_and(|bytes| bytes > 0));
    }

    /// The exchange primitive really is a two-path swap: contents trade
    /// places in one call — the property the no-empty-instant guarantee
    /// rests on.
    #[test]
    fn exchange_trades_both_paths_in_place() {
        let root = tempfile::tempdir().expect("tempdir");
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir(&a).expect("mkdir");
        fs::create_dir(&b).expect("mkdir");
        fs::write(a.join("marker"), b"new").expect("write");
        fs::write(b.join("marker"), b"old").expect("write");

        exchange(&a, &b).expect("exchange");

        assert_eq!(fs::read(a.join("marker")).expect("read"), b"old");
        assert_eq!(fs::read(b.join("marker")).expect("read"), b"new");
    }

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
