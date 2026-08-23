//! Bundle placement primitives: signature-preserving copy, atomic exchange,
//! and the disk-space probe that should run before either.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::exec::run_bounded;
use crate::verify::first_meaningful_line;
use crate::TrustError;

const DITTO_TIMEOUT: Duration = Duration::from_secs(180);

/// Copy `src` to `dst` with `/usr/bin/ditto`.
///
/// `ditto` preserves signatures, resource forks, and xattrs — `cp -R`
/// famously does not, and a stripped signature would fail any re-verify.
pub fn ditto_copy(src: &Path, dst: &Path) -> Result<(), TrustError> {
    let out = run_bounded(
        Path::new("/usr/bin/ditto"),
        &[&src.display().to_string(), &dst.display().to_string()],
        DITTO_TIMEOUT,
    )?;
    if out.success() {
        return Ok(());
    }
    Err(TrustError::DittoFailed {
        detail: first_meaningful_line(&out.stderr),
    })
}

/// Atomically exchange two paths — `renamex_np(RENAME_SWAP)`. Both must
/// exist; afterwards each sits where the other was, with no intermediate
/// state visible to any observer.
#[cfg(target_os = "macos")]
pub fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
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

/// Non-macOS fallback (consumers only ship on macOS; keeps cross-checks
/// compiling): rename through a sibling temp name — a brief empty instant the
/// macOS path does not have.
#[cfg(not(target_os = "macos"))]
pub fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    let hold = b.with_extension("exchange-hold");
    fs::rename(b, &hold)?;
    fs::rename(a, b)?;
    fs::rename(&hold, a)
}

/// What `ensure_disk_space` found when it refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskShortfall {
    pub needed_mb: u64,
    pub free_mb: u64,
}

/// Refuse an operation that would need more bytes than the volume has free.
///
/// A `statvfs` failure is not evidence of a full disk — the caller's own copy
/// step fails safe if space actually runs out — so an unreadable probe passes.
pub fn ensure_disk_space(root: &Path, needed: u64) -> Result<(), DiskShortfall> {
    let Some(free) = free_bytes(root) else {
        return Ok(());
    };
    if free < needed {
        return Err(DiskShortfall {
            needed_mb: needed / (1024 * 1024),
            free_mb: free / (1024 * 1024),
        });
    }
    Ok(())
}

pub fn dir_size(path: &Path) -> u64 {
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
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } == 0;
    ok.then(|| vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
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

    // Shells out to a macOS-only binary, so it can only run there. The
    // crate is a macOS-only dependency; it stays a workspace member so it
    // keeps compiling everywhere, which is why the gate is per-test.
    #[cfg(target_os = "macos")]
    #[test]
    fn free_bytes_reports_something_for_the_temp_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(free_bytes(dir.path()).is_some_and(|bytes| bytes > 0));
    }

    // Shells out to a macOS-only binary, so it can only run there. The
    // crate is a macOS-only dependency; it stays a workspace member so it
    // keeps compiling everywhere, which is why the gate is per-test.
    #[cfg(target_os = "macos")]
    #[test]
    fn ensure_disk_space_refuses_an_impossible_ask() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ensure_disk_space(dir.path(), u64::MAX).expect_err("must refuse");
        assert!(err.free_mb < err.needed_mb);
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

    // Shells out to a macOS-only binary, so it can only run there. The
    // crate is a macOS-only dependency; it stays a workspace member so it
    // keeps compiling everywhere, which is why the gate is per-test.
    #[cfg(target_os = "macos")]
    #[test]
    fn ditto_copies_a_directory_tree() {
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("src");
        let dst = root.path().join("dst");
        fs::create_dir_all(src.join("nested")).expect("mkdir");
        fs::write(src.join("nested/file"), b"payload").expect("write");

        ditto_copy(&src, &dst).expect("ditto");

        assert_eq!(fs::read(dst.join("nested/file")).expect("read"), b"payload");
    }
}
